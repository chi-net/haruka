use axum::http::StatusCode;
use sea_orm::{
    sea_query::Expr, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QuerySelect,
};

use crate::{crypto, entity::meta, AppState};

const ATTEMPTS_PER_RUN: u8 = 10;
const MAX_FAILED_ROUNDS: i32 = 3;

type VerificationResult<T> = Result<T, (StatusCode, String)>;

#[derive(Default)]
pub(crate) struct Attempts {
    failures: u8,
    completed_rounds: Option<i32>,
    // Retain the exhausted round until its durable marker has been written.
    pending_round: Option<i32>,
}

pub(crate) enum Verification {
    Verified(crypto::Dek),
    Incorrect { remaining: u8 },
}

fn storage_error(error: impl std::fmt::Display) -> (StatusCode, String) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("密码验证状态存储失败：{error}"),
    )
}

fn validate_rounds(rounds: i32) -> VerificationResult<()> {
    if !(0..=MAX_FAILED_ROUNDS).contains(&rounds) {
        return Err(storage_error("失败轮次标记无效，请管理员检查数据库"));
    }
    Ok(())
}

fn locked_message(rounds: i32) -> String {
    if rounds >= MAX_FAILED_ROUNDS {
        "密码验证已永久锁定：已耗尽 3 轮，每轮 10 次失败。重启不会解除；请管理员停服清除密码失败轮次标记后再启动。数据库未删除。".into()
    } else {
        format!(
            "密码验证已锁定：本次运行累计 10 次失败，已耗尽 {rounds}/3 轮。请重启服务后再试；第三轮耗尽后将永久锁定。"
        )
    }
}

impl Attempts {
    async fn persist_pending(&mut self, db: &DatabaseConnection) -> VerificationResult<()> {
        let Some(rounds) = self.pending_round else {
            return Ok(());
        };
        let result = meta::Entity::update_many()
            .col_expr(meta::Column::FailedPasswordRounds, Expr::value(rounds))
            .filter(meta::Column::Id.eq(1))
            .exec(db)
            .await
            .map_err(storage_error)?;
        if result.rows_affected != 1 {
            return Err(storage_error("无法写入失败轮次标记，密码验证保持锁定"));
        }
        self.pending_round = None;
        self.completed_rounds = Some(rounds);
        Ok(())
    }
}

pub(crate) async fn lock_message(state: &AppState) -> VerificationResult<Option<String>> {
    let mut attempts = state.password_attempts.lock().await;
    attempts.persist_pending(&state.db).await?;
    let rounds = match attempts.completed_rounds {
        Some(rounds) => rounds,
        None => {
            let rounds = meta::Entity::find_by_id(1)
                .select_only()
                .column(meta::Column::FailedPasswordRounds)
                .into_tuple::<i32>()
                .one(&state.db)
                .await
                .map_err(storage_error)?
                .ok_or((StatusCode::NOT_FOUND, "尚未设置密码".into()))?;
            validate_rounds(rounds)?;
            attempts.completed_rounds = Some(rounds);
            rounds
        }
    };
    Ok(
        (attempts.failures >= ATTEMPTS_PER_RUN || rounds >= MAX_FAILED_ROUNDS)
            .then(|| locked_message(rounds)),
    )
}

pub(crate) async fn verify(state: &AppState, password: &str) -> VerificationResult<Verification> {
    // Hold one shared gate through key derivation and round persistence. Concurrent
    // requests cannot perform extra verifications after the tenth failure.
    let mut attempts = state.password_attempts.lock().await;
    attempts.persist_pending(&state.db).await?;
    if let Some(rounds) = attempts.completed_rounds {
        if attempts.failures >= ATTEMPTS_PER_RUN || rounds >= MAX_FAILED_ROUNDS {
            return Err((StatusCode::TOO_MANY_REQUESTS, locked_message(rounds)));
        }
    }
    let metadata = meta::Entity::find_by_id(1)
        .one(&state.db)
        .await
        .map_err(storage_error)?
        .ok_or((StatusCode::NOT_FOUND, "尚未设置密码".into()))?;
    validate_rounds(metadata.failed_password_rounds)?;
    attempts.completed_rounds = Some(metadata.failed_password_rounds);
    if attempts.failures >= ATTEMPTS_PER_RUN || metadata.failed_password_rounds >= MAX_FAILED_ROUNDS
    {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            locked_message(metadata.failed_password_rounds),
        ));
    }

    let kek = crypto::derive_kek(password, &metadata.salt);
    if let Some(dek) =
        crypto::unwrap_dek(&metadata.dek_nonce, &metadata.wrapped_dek, kek.as_slice())
    {
        return Ok(Verification::Verified(dek));
    }

    attempts.failures += 1;
    if attempts.failures == ATTEMPTS_PER_RUN {
        let rounds = metadata.failed_password_rounds + 1;
        attempts.pending_round = Some(rounds);
        attempts.persist_pending(&state.db).await?;
        return Err((StatusCode::TOO_MANY_REQUESTS, locked_message(rounds)));
    }
    Ok(Verification::Incorrect {
        remaining: ATTEMPTS_PER_RUN - attempts.failures,
    })
}
