use crate::events::{self, InvoiceAmountUpdatedEvent};
use crate::validation::{
    require_admin, require_expiry_not_too_long, require_hash_not_too_long, require_not_paused,
    require_positive_amount, require_usdc_precision, require_valid_payment_link_hash,
};
use crate::{append_history, pending_index_add, pending_index_remove};
use crate::{
    DataKey, Invoice, InvoiceContract, InvoiceContractArgs, InvoiceContractClient, InvoiceError,
    InvoiceStatus, InvoiceSummary, MaybeAddress, MaybeBytes,
};
use soroban_sdk::{contractimpl, Address, Env, Vec};

/// #556: upper bound for the configurable late fee, in basis points (10%).
pub const MAX_LATE_FEE_BPS: u32 = 1_000;

#[contractimpl]
impl InvoiceContract {
    // --- #58: merchant invoice nonce ---

    /// Create an invoice with an optional merchant-supplied nonce for idempotency.
    /// Pass `merchant_nonce = 0` to skip nonce enforcement.
    /// A non-zero nonce that has already been used for this merchant is rejected.
    #[allow(clippy::too_many_arguments)]
    pub fn create_invoice(
        env: Env,
        merchant: Address,
        amount_usdc: i128,
        gross_usdc: i128,
        expires_in_seconds: u64,
        metadata_hash: MaybeBytes,
        payment_link_hash: MaybeBytes,
        merchant_nonce: u64,
        token_address: MaybeAddress,
    ) -> Result<u64, InvoiceError> {
        merchant.require_auth();
        require_not_paused(&env)?;
        require_positive_amount(amount_usdc, gross_usdc)?;
        // #57: USDC decimal precision guardrail
        require_usdc_precision(amount_usdc, gross_usdc)?;
        require_hash_not_too_long(&metadata_hash)?;
        require_hash_not_too_long(&payment_link_hash)?;
        // #16: payment_link_hash must be exactly 32 bytes when provided
        require_valid_payment_link_hash(&payment_link_hash)?;

        if expires_in_seconds == 0 {
            return Err(InvoiceError::ZeroDuration);
        }
        require_expiry_not_too_long(expires_in_seconds)?;

        // #58: reject duplicate merchant nonce
        if merchant_nonce != 0 {
            let nonce_key = DataKey::MerchantNonce(merchant.clone(), merchant_nonce);
            if env.storage().persistent().has(&nonce_key) {
                return Err(InvoiceError::DuplicateNonce);
            }
        }

        let count: u64 = env
            .storage()
            .instance()
            .get(&DataKey::InvoiceCount)
            .unwrap_or(0);
        let id = count
            .checked_add(1)
            .ok_or(InvoiceError::InvoiceCountOverflow)?;
        let expires_at = env
            .ledger()
            .timestamp()
            .checked_add(expires_in_seconds)
            .ok_or(InvoiceError::ExpiryOverflow)?;
        if merchant_nonce != 0 {
            env.storage().persistent().set(
                &DataKey::MerchantNonce(merchant.clone(), merchant_nonce),
                &true,
            );
        }
        let invoice = Invoice {
            id,
            merchant: merchant.clone(),
            amount_usdc,
            gross_usdc,
            status: InvoiceStatus::Pending,
            expires_at,
            paid_at: None,
            payer: MaybeAddress::None,
            metadata_hash,
            payment_link_hash,
            merchant_nonce,
            token_address,
        };

        env.storage()
            .persistent()
            .set(&DataKey::Invoice(id), &invoice);
        env.storage().instance().set(&DataKey::InvoiceCount, &id);

        let merchant_count_key = DataKey::MerchantInvoiceCount(merchant.clone());
        let merchant_count: u64 = env
            .storage()
            .persistent()
            .get(&merchant_count_key)
            .unwrap_or(0);
        env.storage().persistent().set(
            &DataKey::MerchantInvoiceIndex(merchant.clone(), merchant_count),
            &id,
        );
        env.storage()
            .persistent()
            .set(&merchant_count_key, &(merchant_count + 1));

        pending_index_add(&env, id);
        events::invoice_created(&env, id, &invoice);
        Ok(id)
    }

    /// #558: lightweight invoice view returning only the essentials
    /// (id, status, amount and expiry) for list views. Reads from the same
    /// `DataKey::Invoice` storage as `get_invoice`, so it can never go out of
    /// sync with the full record.
    pub fn get_invoice_summary(env: Env, id: u64) -> Result<InvoiceSummary, InvoiceError> {
        let invoice: Invoice = env
            .storage()
            .persistent()
            .get(&DataKey::Invoice(id))
            .ok_or(InvoiceError::NotFound)?;
        Ok(InvoiceSummary {
            id: invoice.id,
            status: invoice.status,
            amount_usdc: invoice.amount_usdc,
            expires_at: invoice.expires_at,
        })
    }

    /// #556: configure the late fee (in basis points) applied to payments
    /// settled inside the grace window after expiry. Admin-only. The value is
    /// bounded by `MAX_LATE_FEE_BPS`; `0` disables the fee.
    pub fn set_late_fee_bps(env: Env, admin: Address, late_fee_bps: u32) -> Result<(), InvoiceError> {
        require_admin(&env, &admin)?;
        require_not_paused(&env)?;
        if late_fee_bps > MAX_LATE_FEE_BPS {
            return Err(InvoiceError::LateFeeTooHigh);
        }
        env.storage()
            .instance()
            .set(&DataKey::LateFeeBps, &late_fee_bps);
        Ok(())
    }

    /// #556: read the currently configured late fee in basis points.
    pub fn get_late_fee_bps(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::LateFeeBps)
            .unwrap_or(0u32)
    }

    pub fn mark_paid(
        env: Env,
        admin: Address,
        id: u64,
        payer: Address,
        provided_metadata_hash: MaybeBytes,
        payment_token: MaybeAddress,
    ) -> Result<(), InvoiceError> {
        require_admin(&env, &admin)?;
        require_not_paused(&env)?;

        let mut invoice: Invoice = env
            .storage()
            .persistent()
            .get(&DataKey::Invoice(id))
            .ok_or(InvoiceError::NotFound)?;

        if invoice.status != InvoiceStatus::Pending {
            return Err(InvoiceError::NotPending);
        }

        if provided_metadata_hash != MaybeBytes::None
            && provided_metadata_hash != invoice.metadata_hash
        {
            return Err(InvoiceError::MetadataMismatch);
        }

        if let MaybeAddress::Some(expected) = &invoice.token_address {
            if payment_token != MaybeAddress::Some(expected.clone()) {
                return Err(InvoiceError::TokenMismatch);
            }
        }

        // #55: apply grace window — payment is valid up to expires_at + grace_window
        let grace: u64 = env
            .storage()
            .instance()
            .get(&DataKey::GraceWindow)
            .unwrap_or(0u64);
        let effective_deadline = invoice
            .expires_at
            .checked_add(grace)
            .unwrap_or(invoice.expires_at);
        let now = env.ledger().timestamp();
        if now >= effective_deadline {
            return Err(InvoiceError::Expired);
        }

        // #556: apply the merchant-configured late fee only when the payment
        // lands inside the grace window (i.e. after expiry but before the
        // effective deadline). On-time payments are never charged a fee.
        if now > invoice.expires_at {
            let late_fee_bps: u32 = env
                .storage()
                .instance()
                .get(&DataKey::LateFeeBps)
                .unwrap_or(0u32);
            if late_fee_bps > 0 {
                let fee = invoice
                    .amount_usdc
                    .checked_mul(late_fee_bps as i128)
                    .ok_or(InvoiceError::AmountOverflow)?
                    / 10_000i128;
                invoice.amount_usdc = invoice
                    .amount_usdc
                    .checked_add(fee)
                    .ok_or(InvoiceError::AmountOverflow)?;
            }
        }

        invoice.status = InvoiceStatus::Paid;
        invoice.paid_at = Some(now);
        invoice.payer = MaybeAddress::Some(payer);
        env.storage()
            .persistent()
            .set(&DataKey::Invoice(id), &invoice);
        pending_index_remove(&env, id);
        append_history(&env, id, InvoiceStatus::Pending, InvoiceStatus::Paid);
        events::invoice_paid(&env, id, &invoice);
        Ok(())
    }

    // --- #56: escrow release entrypoint ---

    /// Release escrow for a paid invoice. Admin-only. Transitions Paid → Released.
    pub fn release_escrow(env: Env, admin: Address, id: u64) -> Result<(), InvoiceError> {
        require_admin(&env, &admin)?;
        require_not_paused(&env)?;

        let mut invoice: Invoice = env
            .storage()
            .persistent()
            .get(&DataKey::Invoice(id))
            .ok_or(InvoiceError::NotFound)?;

        if invoice.status != InvoiceStatus::Paid {
            return Err(InvoiceError::NotPaid);
        }

        invoice.status = InvoiceStatus::Released;
        env.storage()
            .persistent()
            .set(&DataKey::Invoice(id), &invoice);
        append_history(&env, id, InvoiceStatus::Paid, InvoiceStatus::Released);
        events::invoice_released(&env, id, &invoice);
        Ok(())
    }
}
