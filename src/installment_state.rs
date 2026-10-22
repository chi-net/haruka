use crate::{
    crypto,
    entity::{bill, installment_plan},
};
use std::collections::HashMap;

pub(crate) struct RefundContext {
    originals: HashMap<i64, i64>,
    totals: HashMap<i64, i64>,
}

pub(crate) struct RefundState {
    pub refunded: i64,
    pub full_refund: bool,
    pub needs_review: bool,
}

impl RefundContext {
    pub(crate) fn new<'a>(
        dek: &crypto::Dek,
        bills: impl IntoIterator<Item = &'a bill::Model>,
    ) -> Result<Self, String> {
        let mut originals = HashMap::new();
        let mut totals = HashMap::new();
        for bill in bills {
            let amount = crypto::decrypt_cents(dek, &bill.amount);
            if amount < 0 {
                return Err("退款金额不能为负数".into());
            }
            if let Some(id) = bill.refund_of_bill_id {
                let total = totals.entry(id).or_insert(0i64);
                *total = total.checked_add(amount).ok_or("退款金额超出范围")?;
            } else {
                originals.insert(bill.id, amount);
            }
        }
        Ok(Self { originals, totals })
    }

    pub(crate) fn state(
        &self,
        dek: &crypto::Dek,
        plan: &installment_plan::Model,
    ) -> Result<RefundState, String> {
        let Some(id) = plan.bill_id else {
            return Ok(RefundState {
                refunded: 0,
                full_refund: false,
                needs_review: false,
            });
        };
        let original = self.originals.get(&id).ok_or("分期原始账单不存在")?;
        let refunded = self.totals.get(&id).copied().unwrap_or(0);
        let reviewed = if plan.refund_reviewed_amount.is_empty() {
            0
        } else {
            crypto::decrypt_cents(dek, &plan.refund_reviewed_amount)
        };
        if reviewed < 0 {
            return Err("已核对退款金额不正确".into());
        }
        let full_refund = refunded > 0 && refunded >= *original;
        Ok(RefundState {
            refunded,
            full_refund,
            needs_review: !full_refund && refunded != reviewed,
        })
    }
}
