> **Status: considered, not adopted — historical.** Do not treat this as the shipped
> design. The stack here (Postgres/JSON documents) was replaced by embedded SQLite;
> the sibling banner at §3 already records that. It is kept, and deliberately not
> deleted, because it is the **only written record** of two things the current
> decisions still depend on: the global crypto rail intent (NOWPayments, stablecoins)
> and the **PPh 22 0.21%** reference. Its **USD 2.00 crypto deposit minimum is the
> likely origin of the USD 2.00 wind-down payout floor** in
> [`decisions.md`](decisions.md) §Money. Also note its QRIS minimum (Rp 10.000)
> disagrees with `min_first_deposit = 50000` in `config/apikita.toml`.
>
> Referenced from exactly one place: the register's wind-down rows.

this is rough plan need to be audited:

# Product Specification & Architecture Document: SaaS Top-Up Billing System
**Project Name:** LLM API Token SaaS Billing Platform
**Base Currency:** IDR (Indonesian Rupiah)
**Target Market:** Local (Indonesia via QRIS) & Global (International via BNB/USDT Crypto)

---

## 1. System Overview & Constraints

The platform provides a consumption-based LLM API Token service. Instead of direct pay-per-call processing, the system utilizes a **Prepaid Deposit Balance (Top-Up) model** to prevent micro-transaction fatigue and bypass minimum transaction thresholds on crypto networks. 

### Core Business Logic Constraints
* **Single Source of Truth Currency:** All user balances are saved, updated, and logged inside the database strictly in **IDR**.
* **Localization & Multi-Language Support:** 
  * `ID Mode`: The UI presents the balance and pricing natively in IDR (e.g., `Rp 150.000`).
  * `EN Mode`: The UI dynamically visualizes the computed IDR values in USD (e.g., `$10.00 USD`) using a dynamic exchange rate API layer.
* **Payment Routing Matrix:**
  * **Local Users:** Processed through **Midtrans (QRIS)**. Minimum transaction: **Rp 10.000**.
  * **Global Users:** Processed through **NOWPayments (BNB / Stablecoins)**. Minimum transaction: **\$2.00 USD**.

---

## 2. Dynamic Currency Visualizer Architecture (Frontend)

To keep user experience seamless, currency shifting happens entirely on the client side (Frontend) based on the state of the active language toggle (`lang === 'EN'`).

### Multi-Language State Conversion Workflow
1. Fetch the user's base balance in IDR from the backend (`GET /api/user/profile`).
2. Fetch or initialize the cached fiat conversion rate (`1 USD to IDR`).
3. If language state is `EN`, render the balance computed via: `Calculated USD = (IDR Balance / Current Exchange Rate)`.
4. Format decimals conditionally: `IDR` cuts off floating points (integers), while `USD` retains exactly 2 decimal points.

---

## 3. Database Schema Blueprint (illustrative — not the shipped schema)

> **This section is a proposal, not the data model.** The shipped schema is
> [`server/migrations/20260925000000_initial_schema.sql`](../server/migrations/20260925000000_initial_schema.sql)
> (embedded SQLite, every table `STRICT`); the design record is
> [`docs/website/02-data-model.md`](website/02-data-model.md). The JSON documents
> below describe a document-store shape (Postgres `JSONB` or MongoDB) that was
> considered for this billing prototype and **was not adopted** — the real model is
> relational, with money in `wallets.balance_idr` and `ledger.delta_idr`.`

### `users` Collection / Table
Stores core identities and the single source of truth balance metrics.
```json
{
  "_id": "user_alphanumeric_id",
  "email": "developer@globaltech.com",
  "balance_idr": 150000, 
  "created_at": "2026-09-26T10:00:00Z"
}
```

### `transaction_logs` Collection / Table
Auditable historical ledgers used natively for calculating corporate monthly revenue tax.
```json
{
  "_id": "tx_alphanumeric_id",
  "user_id": "user_alphanumeric_id",
  "order_id": "TOPUP-CRYPTO-171654321",
  "payment_gateway": "NOWPayments",
  "payment_method": "BNB",
  "amount_foreign": 2.00,
  "currency_foreign": "USD",
  "exchange_rate_used": 15000.00,
  "amount_settled_idr": 30000,
  "status": "COMPLETED",
  "created_at": "2026-09-26T10:05:00Z"
}
```

---

## 4. Payment & Webhook Integration Layer (Backend Node.js)

```javascript
const express = require('express');
const axios = require('axios');
const app = express();
app.use(express.json());

// Mock Environment Variables & Cache Configurations
const MIDTRANS_SNAP_URL = 'https://midtrans.com'; // Change to production in live
const NOWPAYMENTS_API_URL = 'https://nowpayments.io';
const GLOBAL_FX_RATE = 15000.00; // Updated periodically via cron jobs (e.g., ExchangeRate-API)

/**
 * 1. ENDPOINT: CREATE INVOICE INTENT
 */
app.post('/api/billing/topup/create', async (req, res) => {
    try {
        const { userId, inputAmount, method } = req.body; 
        // inputAmount is structural: in IDR if method is QRIS, in USD if method is BNB.

        if (method === 'QRIS') {
            const amountInIDR = parseFloat(inputAmount);
            if (amountInIDR < 10000) {
                return res.status(400).json({ error: "Minimum Top-Up for QRIS payments is Rp 10.000" });
            }

            // Execute Midtrans Token Handshake
            const midtransPayload = {
                transaction_details: { order_id: `TOPUP-QRIS-${Date.now()}`, gross_amount: amountInIDR },
                credit_card: { secure: true }
            };
            const response = await axios.post(MIDTRANS_SNAP_URL, midtransPayload, {
                headers: { 'Authorization': `Basic ${Buffer.from(process.env.MIDTRANS_SERVER_KEY + ':').toString('base64')}` }
            });
            return res.status(200).json({ paymentUrl: response.data.redirect_url });
        } 

        if (method === 'BNB') {
            const amountInUSD = parseFloat(inputAmount);
            if (amountInUSD < 2.00) {
                return res.status(400).json({ error: "Minimum Top-Up for BNB Crypto transactions is \$2.00 USD" });
            }

            // Execute NOWPayments Invoice Handshake
            const cryptoPayload = {
                price_amount: amountInUSD,
                price_currency: 'usd',
                pay_currency: 'bnb',
                order_id: `TOPUP-CRYPTO-${Date.now()}`,
                is_fee_paid_by_user: true // Outsource gas and transaction network fees to the global client
            };
            const response = await axios.post(NOWPAYMENTS_API_URL, cryptoPayload, {
                headers: { 'x-api-key': process.env.NOWPAYMENTS_API_KEY, 'Content-Type': 'application/json' }
            });
            return res.status(200).json({ paymentUrl: response.data.invoice_url });
        }

        return res.status(400).json({ error: "Unsupported transaction protocol." });
    } catch (error) {
        return res.status(500).json({ error: error.message });
    }
});

/**
 * 2. ENDPOINT: NOWPAYMENTS CRYPTO WEBHOOK RESOLVER
 */
app.post('/api/billing/webhook/nowpayments', async (req, res) => {
    // Note: Always validate 'x-nowpayments-sig' signature headers in production environment!
    const { payment_status, price_amount, order_id } = req.body;

    if (payment_status === 'finished') {
        const amountInUSD = parseFloat(price_amount);
        const equivalentIDR = amountInUSD * GLOBAL_FX_RATE;

        // Perform atomic adjustments to secure financial ledgers
        await db.users.updateOne({ _id: req.body.userId }, { \$inc: { balance_idr: equivalentIDR } });
        await db.transaction_logs.insertOne({
            order_id, payment_gateway: 'NOWPayments', payment_method: 'BNB',
            amount_foreign: amountInUSD, currency_foreign: 'USD',
            exchange_rate_used: GLOBAL_FX_RATE, amount_settled_idr: equivalentIDR,
            status: 'COMPLETED', created_at: new Date()
        });
        return res.status(200).send('Crypto payload integrated.');
    }
    return res.status(200).send('State tracked but skipped.');
});
```

---

## 5. Indonesian Tax Compliance & Off-Ramping

Because the core database schema continuously standardizes financial data into Indonesian Rupiah (IDR), tax tracking is direct:

1. **Monthly Corporate/Individual Revenue (Gross Turnover):** Run an aggregate script over `transaction_logs` matching a monthly timeline. The sum of `amount_settled_idr` gives the exact gross revenue. Under **PP 55/2022**, if qualified as an individual MSME (UMKM), calculate a clean **0.5% PPh Final** off this number.
2. **Crypto Off-Ramp Protocol (Compliance with PMK 50/2025):** 
   When NOWPayments auto-forwards the client's BNB/USDT payments to your local OJK/Bappebti registered exchange (e.g., Tokocrypto, Indodax), the subsequent fiat liquidations to an Indonesian bank account will automatically trigger a **0.21% PPh Pasal 22 Final** deduction handled seamlessly by the exchange platform.


# Model Pricing Conversion Blueprint

## 1. Static Configuration Matrix (Backend/Config Level)
To prevent drift, model base pricing is defined uniformly per 1,000,000 (1M) tokens in IDR.

```json
{
  "models": [
    {
      "id": "llm-fast-v1",
      "name": "LLM Fast (Standard)",
      "price_per_1m_input_idr": 15000,
      "price_per_1m_output_idr": 30000
    },
    {
      "id": "llm-reasoning-v1",
      "name": "LLM Reasoning (Advanced)",
      "price_per_1m_input_idr": 45000,
      "price_per_1m_output_idr": 90000
    }
  ]
}
```

## 2. Frontend React Implementation Strategy

This single component contains the currency conversion logic, live FX rate handling framework, and language configuration routing.

```tsx
import React, { useState, useEffect } from 'react';

// Interfaces for structured pricing models
interface ModelPrice {
  id: string;
  name: string;
  price_per_1m_input_idr: number;
  price_per_1m_output_idr: number;
}

export const PricingDashboard: React.FC = () => {
  // 1. Core State Managers
  const [lang, setLang] = useState<'ID' | 'EN'>('EN');
  const [usdToIdrRate, setUsdToIdrRate] = useState<number>(15000); // Dynamic fallback multiplier
  
  // Mock data representing database configurations
  const modelRegistry: ModelPrice[] = [
    { id: "llm-fast-v1", name: "LLM Fast (Standard)", price_per_1m_input_idr: 15000, price_per_1m_output_idr: 30000 },
    { id: "llm-reasoning-v1", name: "LLM Reasoning (Advanced)", price_per_1m_input_idr: 45000, price_per_1m_output_idr: 90000 }
  ];

  // 2. Fetch Active Foreign Exchange Rates Automatically on Mount
  useEffect(() => {
    const fetchCurrentFxRate = async () => {
      try {
        // Utilizing a lightweight open access currency engine
        const response = await fetch('https://er-api.com');
        const data = await response.json();
        if (data && data.rates && data.rates.IDR) {
          setUsdToIdrRate(data.rates.IDR);
        }
      } catch (error) {
        console.error("FX synchronization failed. Utilizing local fallback matrix.", error);
      }
    };
    fetchCurrentFxRate();
  }, []);

  // 3. Mathematical Formatting Logic Engine
  const formatPrice = (amountInIdr: number) => {
    if (lang === 'ID') {
      // Return integer string formatted natively in Indonesian Rupiah
      return new Intl.NumberFormat('id-ID', {
        style: 'currency',
        currency: 'IDR',
        maximumFractionDigits: 0
      }).format(amountInIdr);
    } else {
      // Calculate equivalent asset values against modern spot pricing structures
      const amountInUsd = amountInIdr / usdToIdrRate;
      return new Intl.NumberFormat('en-US', {
        style: 'currency',
        currency: 'USD',
        minimumFractionDigits: 2,
        maximumFractionDigits: 4 // Expanded decimals to support micro token metering
      }).format(amountInUsd);
    }
  };

  return (
    <div style={{ padding: '24px', fontFamily: 'sans-serif', maxWidth: '800px', margin: '0 auto' }}>
      {/* Dynamic Header Toggle Controllers */}
      <div style={{ display: 'flex', justifyContent: 'space-between', marginBottom: '32px' }}>
        <h2>{lang === 'EN' ? 'Model Interface Pricing' : 'Harga Interface Model'}</h2>
        <button 
          onClick={() => setLang(lang === 'EN' ? 'ID' : 'EN')}
          style={{ padding: '8px 16px', cursor: 'pointer', fontWeight: 'bold' }}
        >
          🌐 Switch to {lang === 'EN' ? 'Bahasa Indonesia (IDR)' : 'English (USD)'}
        </button>
      </div>

      {/* Dynamic Data Presentation Layer */}
      <div style={{ display: 'grid', gap: '20px', gridTemplateColumns: '1fr 1fr' }}>
        {modelRegistry.map((model) => (
          <div key={model.id} style={{ border: '1px solid #ccc', padding: '20px', borderRadius: '8px' }}>
            <h3>{model.name}</h3>
            <hr />
            <p>
              <strong>{lang === 'EN' ? 'Input Cost (per 1M tokens):' : 'Biaya Input (per 1M token):'}</strong><br />
              <span style={{ fontSize: '1.25rem', color: '#0070f3' }}>{formatPrice(model.price_per_1m_input_idr)}</span>
            </p>
            <p>
              <strong>{lang === 'EN' ? 'Output Cost (per 1M tokens):' : 'Biaya Output (per 1M token):'}</strong><br />
              <span style={{ fontSize: '1.25rem', color: '#0070f3' }}>{formatPrice(model.price_per_1m_output_idr)}</span>
            </p>
          </div>
        ))}
      </div>
    </div>
  );
};
```
