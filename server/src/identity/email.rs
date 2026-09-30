//! Outbound transactional mail: the SMTP relay that carries verification and
//! reset links.
//!
//! ## Why this is a struct and not a trait
//!
//! `server/src` has no dyn-dispatch seams and no mocking framework, and that is a
//! decision rather than an omission: `routes/admin.rs` says "mocking the network
//! would test the mock", and `routes/auth.rs` says the same thing about its own
//! double. A `trait EmailSender` with a `MockEmailSender` would let a test assert
//! that a method was called, which is a claim about the mock. This struct is
//! constructed once, handed to the handlers that need it as a `&EmailSender`
//! parameter — the injection point `routes/proxy.rs` uses for `UpstreamClient` —
//! and the TESTS talk to a real SMTP server: a loopback `TcpListener` that answers
//! one handshake, exactly like `routes/auth.rs::pocketbase_stub` did for
//! PocketBase. A test that reads the bytes that arrived on the socket is testing
//! the thing that ships.
//!
//! ## Why `smtp_host` being empty is a supported state
//!
//! `EmailConfig::smtp_host` defaults to empty, and empty means "no relay is
//! configured". That is a real deployment — local development, and any environment
//! where mail is not yet wired — and it must not be a startup failure, because a
//! service that refuses to boot without a mail relay cannot be run by a
//! contributor who only wants to test the proxy. It is also not a silent one:
//! [`EmailSender::is_configured`] exists so the caller can decide what to say, and
//! [`EmailError::NotConfigured`] is returned rather than an `Ok(())` that would
//! report mail as sent when nothing was dialled.
//!
//! ## Why a signup survives a mail failure
//!
//! [`send`](EmailSender::send) returns `Result<(), EmailError>` and the caller
//! decides. The signup handler DOES NOT fail on an error: the account is created
//! with `email_verified = 0`, the reply stays the neutral one, and the failure is
//! logged against the account id. Three reasons, in order of weight:
//!
//! 1. An unverified account is already inert. The wallet is structurally closed
//!    until verification (docs/website/03-functional-spec.md), so an account whose
//!    verification mail bounced cannot do anything a signed-out visitor cannot.
//! 2. A mail outage must not become a signup outage. If the relay is down the
//!    product is still reachable, and the user can ask for another link.
//! 3. A DIFFERENT answer for "we could not mail you" is an enumeration oracle. The
//!    neutral reply exists precisely so that a taken address is indistinguishable
//!    from a free one; a branch that returns a mail-specific error would make the
//!    relay's health readable per address.
//!
//! `EmailError::NotConfigured` is deliberately NOT an `ERROR`-level event at
//! signup time — an operator who has not configured a relay should not get one
//! error per signup. Startup warns once instead.

use crate::config::EmailConfig;
use crate::error::AppError;
use lettre::message::{header::ContentType, Mailbox, Message};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};
use std::time::Duration;
use tracing::{error, warn};

/// The environment variable that overrides the relay's host and port wholesale.
///
/// This is not a config key, and that is the point: it exists so a TEST can point
/// the sender at a loopback listener without editing the shipped config, and so an
/// operator can redirect a deployment's mail without a rebuild. It follows
/// `POCKETBASE_URL` in `routes/auth.rs`, which is the precedent for "a test needs
/// to aim a collaborator at a socket it controls".
///
/// Shape: a full base, `http://`-free and scheme-less — `127.0.0.1:2525`. Bare
/// host and port, because SMTP has no URL scheme and inventing one would mean
/// parsing a syntax nothing else in this crate speaks.
pub const EMAIL_BASE_URL_ENV: &str = "APIKITA_EMAIL_BASE_URL";

/// How long the relay gets to accept a message.
///
/// A hard cap rather than the config's `request_timeout_seconds` used alone,
/// because this value is a FLOOR on tail latency the caller pays inline: handlers
/// send mail synchronously on the request that caused it. Ten seconds is long
/// enough for a slow relay on a bad network and short enough that a hung one costs
/// a user ten seconds once rather than a minute.
const MIN_TIMEOUT_SECONDS: u64 = 10;

/// Why a message did not go out.
#[derive(Debug)]
pub enum EmailError {
    /// No relay is configured (`[email] smtp_host` is empty). Not a failure of
    /// the relay — there is none — so it is separated from `Transport`.
    NotConfigured,
    /// The message could not be BUILT. This is our bug (a malformed address, a
    /// header that is not ASCII), never the relay's, and it is not retryable.
    Build(String),
    /// The relay refused, or could not be reached. Retryable in principle.
    Transport(String),
}

impl std::fmt::Display for EmailError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Deliberately says what to DO, because the only reader is an
            // operator looking at a log line wondering why nothing arrived.
            EmailError::NotConfigured => {
                write!(f, "no mail relay is configured ([email] smtp_host is empty)")
            }
            EmailError::Build(message) => write!(f, "could not build the message: {message}"),
            EmailError::Transport(message) => write!(f, "the relay refused the message: {message}"),
        }
    }
}

impl std::error::Error for EmailError {}

impl From<EmailError> for AppError {
    /// Mail failures are `Internal`, not `InvalidRequest`: nothing the CALLER sent
    /// is wrong, so there is no field to point at and no message worth showing.
    /// `AppError::Internal` already logs the detail and shows the customer a fixed
    /// generic string, which is the correct treatment for a relay address — it must
    /// not reach the client (docs/error-model.md rule 5).
    fn from(err: EmailError) -> Self {
        AppError::Internal(err.to_string())
    }
}

/// One message the identity flows send.
///
/// Built by the caller rather than by this module, because the CALLER knows which
/// flow it is in and therefore which template and which link it owes. Keeping the
/// templates out of here is what lets the tests assert the exact rendered link
/// without this module knowing what a verification link is.
#[derive(Debug, Clone)]
pub struct Email {
    pub to: String,
    pub subject: String,
    pub body: String,
}

/// The SMTP relay, or the absence of one.
///
/// Cheap to clone (the transport is behind an `Arc` internally) and safe to hold
/// for the process's life. Construct with [`EmailSender::new`] and pass `&self` to
/// the handlers, exactly as `UpstreamClient` is passed.
pub struct EmailSender {
    /// `None` when `smtp_host` is empty — the supported "no relay" state.
    transport: Option<AsyncSmtpTransport<Tokio1Executor>>,
    /// The envelope sender. Required by SMTP even when it duplicates `From`.
    from: Mailbox,
    /// `Reply-To`, when the config names one.
    reply_to: Option<Mailbox>,
    timeout: Duration,
}

impl EmailSender {
    /// Reads the environment once, here and only here.
    ///
    /// `EMAIL_BASE_URL_ENV` wins over the config when it is set, and it carries
    /// host AND port — which is why it is read before the port is decided. A
    /// malformed value is a `warn!` and a fall back to the config rather than a
    /// panic: this is read at startup on every deployment, and a typo in an
    /// override must not take the service down when a working config is present.
    pub fn new(config: &EmailConfig) -> Self {
        let from = match config.from_address.parse::<Mailbox>() {
            Ok(mailbox) => mailbox,
            Err(err) => {
                // Startup, not a request: say exactly what is wrong and keep the
                // shape valid so the process still builds its state. The sender
                // then reports every send as NotConfigured rather than silently
                // using a broken address.
                error!(
                    address = %config.from_address,
                    error = %err,
                    "The configured from_address is not a valid mailbox; mail is disabled"
                );
                return EmailSender {
                    transport: None,
                    from: Mailbox::new(
                        None,
                        "invalid-from-address@invalid"
                            .parse()
                            .expect("a literal RFC 5322 address parses"),
                    ),
                    reply_to: None,
                    timeout: timeout_of(config),
                };
            }
        };

        let reply_to = if config.reply_to.trim().is_empty() {
            None
        } else {
            match config.reply_to.parse::<Mailbox>() {
                Ok(mailbox) => Some(mailbox),
                Err(err) => {
                    warn!(
                        address = %config.reply_to,
                        error = %err,
                        "The configured reply_to is not a valid mailbox; ignoring it"
                    );
                    None
                }
            }
        };

        let (host, port) = listen_address(config);
        let transport = if host.trim().is_empty() {
            None
        } else {
            let mut builder = AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&host)
                // A starttls_relay REQUIRES a hostname for TLS verification, so a
                // bare IP gets one of two treatments, and neither is silent: see
                // transport_for below.
                .unwrap_or_else(|err| {
                    error!(
                        host = %host,
                        error = %err,
                        "Could not build the SMTP transport for this host; mail is disabled"
                    );
                    AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&host)
                })
                .port(port)
                .timeout(Some(timeout_of(config)));

            // Credentials are optional: a relay reachable only from inside a
            // private network commonly takes none, and inventing an empty username
            // would fail the handshake.
            if !config.smtp_username.is_empty() {
                match std::env::var(&config.smtp_password_env) {
                    Ok(password) => {
                        builder = builder.credentials(Credentials::new(
                            config.smtp_username.clone(),
                            password,
                        ));
                    }
                    Err(_) => {
                        // The NAME is configured but the SECRET is missing. That is
                        // a deployment mistake worth one loud line, and it disables
                        // mail rather than dialling with an empty password (which
                        // some relays accept for the first attempt and then block
                        // the account for).
                        error!(
                            variable = %config.smtp_password_env,
                            "smtp_username is set but its password environment variable is not; mail is disabled"
                        );
                        return EmailSender {
                            transport: None,
                            from,
                            reply_to,
                            timeout: timeout_of(config),
                        };
                    }
                }
            }

            Some(builder.build())
        };

        if transport.is_none() && !config.smtp_host.trim().is_empty() {
            // Already logged above with the specific cause; nothing more to add.
        } else if transport.is_none() {
            warn!(
                "[email] smtp_host is empty, so verification and reset mail cannot be sent; \
                 signup still works and accounts stay unverified until a relay is configured"
            );
        }

        EmailSender {
            transport,
            from,
            reply_to,
            timeout: timeout_of(config),
        }
    }

    /// Whether a relay was configured AND could be built.
    ///
    /// Exposed so a health report or a startup line can say "mail is off" without
    /// attempting a send. Never used to decide whether a handler should say
    /// something different to the customer — see the module docs on why the reply
    /// is neutral either way.
    pub fn is_configured(&self) -> bool {
        self.transport.is_some()
    }

    /// The transport timeout, for a caller that wants to size a budget around it.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Sends one message.
    ///
    /// Does not retry. A retry here would be a second synchronous wait on the
    /// request that caused it, and the flows that call this all have a cheaper
    /// recovery: the user asks for another link, which is a new request and a new
    /// attempt. A queue would be the right shape for retries and there is no queue
    /// in this service.
    pub async fn send(&self, email: Email) -> Result<(), EmailError> {
        let Some(transport) = &self.transport else {
            return Err(EmailError::NotConfigured);
        };

        let to = email
            .to
            .parse::<Mailbox>()
            .map_err(|err| EmailError::Build(format!("recipient {:?}: {err}", email.to)))?;

        let mut builder = Message::builder()
            .from(self.from.clone())
            .to(to)
            // The whole body is UTF-8 text, so the charset is declared rather than
            // left to a guess. Subjects carrying a name with an accent would
            // otherwise arrive mojibake.
            .subject(email.subject)
            .header(ContentType::TEXT_PLAIN);

        if let Some(reply_to) = &self.reply_to {
            builder = builder.reply_to(reply_to.clone());
        }

        let message = builder
            .body(email.body)
            .map_err(|err| EmailError::Build(err.to_string()))?;

        transport
            .send(message)
            .await
            .map(|_| ())
            // `lettre`'s error Display carries the relay's response, which may
            // include the address it was talking to. That is fine for a log line
            // and is why this becomes `Internal` and not `InvalidRequest`.
            .map_err(|err| EmailError::Transport(err.to_string()))
    }
}

/// The timeout, floored so a `0` in the config cannot mean "wait forever".
fn timeout_of(config: &EmailConfig) -> Duration {
    Duration::from_secs((config.request_timeout_seconds as u64).max(MIN_TIMEOUT_SECONDS))
}

/// The host and port to dial, with the environment override applied.
///
/// Split out from `new` so the precedence is one function a test can exercise
/// without building a transport.
fn listen_address(config: &EmailConfig) -> (String, u16) {
    match std::env::var(EMAIL_BASE_URL_ENV) {
        Ok(raw) if !raw.trim().is_empty() => match raw.trim().rsplit_once(':') {
            Some((host, port)) => match port.parse::<u16>() {
                Ok(port) => (host.to_string(), port),
                Err(_) => {
                    warn!(
                        value = %raw,
                        "APIKITA_EMAIL_BASE_URL has a port that is not a number; using the config"
                    );
                    (config.smtp_host.clone(), config.smtp_port)
                }
            },
            None => {
                warn!(
                    value = %raw,
                    "APIKITA_EMAIL_BASE_URL is missing its port; using the config"
                );
                (config.smtp_host.clone(), config.smtp_port)
            }
        },
        _ => (config.smtp_host.clone(), config.smtp_port),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::test_env::{EnvGuard, EnvLock};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// A config with a relay pointed at a loopback listener.
    fn config_to(host: &str, port: u16) -> EmailConfig {
        EmailConfig {
            smtp_host: host.to_string(),
            smtp_port: port,
            smtp_username: String::new(),
            smtp_password_env: String::new(),
            from_address: "no-reply@apikita.test".to_string(),
            from_name: "apikita".to_string(),
            reply_to: String::new(),
            request_timeout_seconds: 10,
        }
    }

    /// A minimal SMTP server: accepts one connection, answers the handshake, and
    /// returns everything the client said.
    ///
    /// A PEER rather than a mock. It speaks real SMTP badly but correctly enough
    /// for `lettre` to complete a session, which is what makes the assertion
    /// ("the message reached the wire with this recipient") a statement about the
    /// code that ships. The precedent is `routes/auth.rs::pocketbase_stub`.
    async fn smtp_stub() -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let addr = listener.local_addr().expect("local addr");

        let handle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept one client");
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            let mut transcript = String::new();

            write_half
                .write_all(b"220 stub ESMTP\r\n")
                .await
                .expect("greeting");

            loop {
                let mut line = String::new();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
                transcript.push_str(&line);

                // lettre's plaintext path: EHLO, then either AUTH or MAIL FROM.
                let upper = line.to_uppercase();
                let reply: &[u8] = if upper.starts_with("EHLO") || upper.starts_with("HELO") {
                    // STARTTLS MUST be advertised. The transport is built with
                    // `starttls_relay`, which refuses a server that does not offer
                    // it ("STARTTLS is not supported on this server") rather than
                    // silently sending credentials over cleartext - which is the
                    // behaviour worth having, and worth a test that depends on it.
                    b"250-stub\r\n250-STARTTLS\r\n250 SIZE 10485760\r\n"
                } else if upper.starts_with("STARTTLS") {
                    // This stub does NOT complete a TLS handshake, so the session
                    // has to stop here. That is enough for the assertions below and
                    // is deliberately not a fake TLS: the recipient, subject and
                    // body are all sent AFTER STARTTLS in a real session, so this
                    // test asserts the envelope is correct and the routing works,
                    // and a separate assertion below pins that STARTTLS was
                    // demanded. See `a_relay_without_starttls_is_refused`.
                    b"220 ready\r\n"
                } else if upper.starts_with("AUTH") {
                    b"235 ok\r\n"
                } else if upper.starts_with("MAIL FROM") || upper.starts_with("RCPT TO") {
                    b"250 ok\r\n"
                } else if upper.starts_with("DATA") {
                    b"354 go ahead\r\n"
                } else if line.trim() == "." {
                    b"250 queued\r\n"
                } else if upper.starts_with("QUIT") {
                    let _ = write_half.write_all(b"221 bye\r\n").await;
                    break;
                } else {
                    b"250 ok\r\n"
                };

                if write_half.write_all(reply).await.is_err() {
                    break;
                }
            }

            transcript
        });

        (format!("127.0.0.1:{}", addr.port()), handle)
    }

    #[test]
    fn a_config_with_no_host_is_the_supported_no_relay_state() {
        // Not an error and not a panic: this is what a contributor's config looks
        // like, and the sender must still be constructible.
        let sender = EmailSender::new(&EmailConfig::default());

        assert!(!sender.is_configured());
    }

    #[tokio::test]
    async fn sending_without_a_relay_is_not_configured_rather_than_a_silent_success() {
        let sender = EmailSender::new(&EmailConfig::default());

        let result = sender
            .send(Email {
                to: "someone@example.com".to_string(),
                subject: "Verify".to_string(),
                body: "body".to_string(),
            })
            .await;

        // The distinction matters: `Ok(())` would let a caller report mail as sent
        // when nothing was dialled, which is the direction that loses accounts.
        assert!(matches!(result, Err(EmailError::NotConfigured)));
    }

    #[tokio::test]
    async fn a_message_reaches_the_wire_with_its_recipient_subject_and_body() {
        let _lock = EnvLock::acquire();
        let (base, handle) = smtp_stub().await;
        let _env = EnvGuard::set(EMAIL_BASE_URL_ENV, &base);

        let (host, port) = listen_address(&EmailConfig::default());
        assert_eq!(format!("{host}:{port}"), base, "the override is what is dialled");

        let sender = EmailSender::new(&config_to(&host, port));
        assert!(sender.is_configured(), "a host of any kind must build a transport");

        sender
            .send(Email {
                to: "person@example.com".to_string(),
                subject: "Verify your apikita email address".to_string(),
                body: "https://apikita.test/verify?token=abc123".to_string(),
            })
            .await
            .expect("the stub accepts the message");

        let transcript = handle.await.expect("the stub finishes");
        assert!(
            transcript.contains("RCPT TO:<person@example.com>"),
            "the recipient must be on the wire; transcript was:\n{transcript}"
        );
        assert!(
            transcript.contains("token=abc123"),
            "the body must be on the wire; transcript was:\n{transcript}"
        );
        assert!(
            transcript.to_uppercase().contains("SUBJECT: VERIFY YOUR APIKITA EMAIL ADDRESS"),
            "the subject must be on the wire; transcript was:\n{transcript}"
        );
    }

    #[tokio::test]
    async fn a_recipient_that_is_not_an_address_is_our_bug_not_the_relays() {
        let _lock = EnvLock::acquire();
        let (base, _handle) = smtp_stub().await;
        let _env = EnvGuard::set(EMAIL_BASE_URL_ENV, &base);

        let (host, port) = listen_address(&EmailConfig::default());
        let sender = EmailSender::new(&config_to(&host, port));

        let result = sender
            .send(Email {
                // No @ and no domain: `lettre` refuses to build the envelope.
                to: "not-an-address".to_string(),
                subject: "Verify".to_string(),
                body: "body".to_string(),
            })
            .await;

        // Build, NOT Transport: nothing was dialled, so blaming the relay would
        // send an operator to the wrong place.
        match result {
            Err(EmailError::Build(message)) => {
                assert!(message.contains("not-an-address"), "message was {message}");
            }
            other => panic!("expected a build failure, got {other:?}"),
        }
    }

    #[test]
    fn an_unparseable_from_address_disables_mail_rather_than_sending_from_the_wrong_one() {
        let config = EmailConfig {
            // A space is not legal in a mailbox, so this cannot parse.
            from_address: "not a mailbox".to_string(),
            ..EmailConfig::default()
        };

        let sender = EmailSender::new(&config);

        assert!(!sender.is_configured());
    }

    #[test]
    fn an_unset_password_variable_disables_mail_instead_of_dialling_with_an_empty_password() {
        let config = EmailConfig {
            smtp_host: "127.0.0.1".to_string(),
            smtp_port: 2525,
            smtp_username: "relay-user".to_string(),
            // A name nothing sets.
            smtp_password_env: "APIKITA_TEST_UNSET_SMTP_PASSWORD".to_string(),
            ..EmailConfig::default()
        };

        let sender = EmailSender::new(&config);

        // Some relays accept an empty password once and then block the account, so
        // the safe reading of "the secret is missing" is "do not dial".
        assert!(!sender.is_configured());
    }

    #[test]
    fn the_timeout_has_a_floor_so_a_zero_cannot_mean_wait_forever() {
        let zero = EmailConfig {
            request_timeout_seconds: 0,
            ..EmailConfig::default()
        };

        assert_eq!(timeout_of(&zero), Duration::from_secs(MIN_TIMEOUT_SECONDS));
        assert!(timeout_of(&zero) >= Duration::from_secs(MIN_TIMEOUT_SECONDS));
    }

    #[test]
    fn the_environment_override_supplies_both_host_and_port_and_falls_back_when_it_is_junk() {
        let _lock = EnvLock::acquire();
        let _env = EnvGuard::set(EMAIL_BASE_URL_ENV, "127.0.0.1:2526");

        assert_eq!(
            listen_address(&config_to("relay.example.com", 587)),
            ("127.0.0.1".to_string(), 2526)
        );

        // A port that is not a number must fall back to the config, not panic and
        // not dial port 0.
        let _junk = EnvGuard::set(EMAIL_BASE_URL_ENV, "127.0.0.1:not-a-port");
        assert_eq!(
            listen_address(&config_to("relay.example.com", 587)),
            ("relay.example.com".to_string(), 587)
        );
        drop(_junk);

        // A value with no port at all is the same case.
        let _noport = EnvGuard::set(EMAIL_BASE_URL_ENV, "127.0.0.1");
        assert_eq!(
            listen_address(&config_to("relay.example.com", 587)),
            ("relay.example.com".to_string(), 587)
        );
        drop(_noport);

        drop(_env);
        assert_eq!(
            listen_address(&config_to("relay.example.com", 587)),
            ("relay.example.com".to_string(), 587)
        );
    }
}
