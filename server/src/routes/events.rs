use axum::{
    extract::State,
    http::{header, HeaderMap},
    response::sse::{Event, KeepAlive, Sse},
};
use futures_util::stream::Stream;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use std::{convert::Infallible, time::Duration};
use tokio_stream::StreamExt;
use uuid::Uuid;

use crate::error::AppError;

fn hash_string(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    hex::encode(hasher.finalize())
}

async fn resolve_account_from_cookie(pool: &PgPool, headers: &HeaderMap) -> Result<Uuid, AppError> {
    let cookie_hdr = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .ok_or(AppError::Unauthenticated)?;

    for piece in cookie_hdr.split(';') {
        let piece = piece.trim();
        if let Some(token) = piece.strip_prefix("session=") {
            let token_hash = hash_string(token);
            let session = sqlx::query(
                "SELECT account_id FROM sessions WHERE token_hash = $1 AND revoked_at IS NULL AND expires_at > now()",
            )
            .bind(token_hash)
            .fetch_optional(pool)
            .await?;

            if let Some(s) = session {
                return Ok(s.get("account_id"));
            }
        }
    }

    Err(AppError::Unauthenticated)
}

pub async fn sse_events_handler(
    State(pool): State<PgPool>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, AppError> {
    let account_id = resolve_account_from_cookie(&pool, &headers).await?;

    let wallet = sqlx::query("SELECT balance_idr FROM wallets WHERE account_id = $1")
        .bind(account_id)
        .fetch_optional(&pool)
        .await?;

    let balance_idr: i64 = wallet.map(|w| w.get("balance_idr")).unwrap_or(0);

    let initial_event = Event::default()
        .event("balance")
        .data(format!(r#"{{"balance_idr": {}}}"#, balance_idr));

    let stream = tokio_stream::iter(vec![Ok(initial_event)]).chain(
        tokio_stream::wrappers::IntervalStream::new(tokio::time::interval(Duration::from_secs(10)))
            .map(move |_| {
                Ok(Event::default()
                    .event("heartbeat")
                    .comment("heartbeat"))
            }),
    );

    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(20))
            .text("heartbeat"),
    ))
}
