use super::*;
use crate::entity::{account_detail, balance_adjustment, category, debt_person, debt_record};
use sea_orm::{ConnectionTrait, Database, DbBackend, PaginatorTrait, Schema, Statement};

struct Fixture {
    state: AppState,
    dek: crypto::Dek,
    source: i64,
    fund: i64,
}

impl Fixture {
    async fn new(balance: i64) -> Self {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let backend = DbBackend::Sqlite;
        let schema = Schema::new(backend);
        for table in [
            schema.create_table_from_entity(account::Entity),
            schema.create_table_from_entity(account_detail::Entity),
            schema.create_table_from_entity(category::Entity),
            schema.create_table_from_entity(bill::Entity),
            schema.create_table_from_entity(transfer::Entity),
            schema.create_table_from_entity(debt_person::Entity),
            schema.create_table_from_entity(debt_record::Entity),
            schema.create_table_from_entity(balance_adjustment::Entity),
            schema.create_table_from_entity(recurring_investment::Entity),
            schema.create_table_from_entity(investment_execution::Entity),
            schema.create_table_from_entity(market_closed_day::Entity),
        ] {
            db.execute(backend.build(&table)).await.unwrap();
        }
        db.execute(Statement::from_string(
            backend,
            "CREATE UNIQUE INDEX idx_investment_execution_plan_date ON investment_executions (plan_id, trade_date)".to_owned(),
        ))
        .await
        .unwrap();
        let webauthn = webauthn_rs::prelude::WebauthnBuilder::new(
            "localhost",
            &"http://localhost:3000".parse().unwrap(),
        )
        .unwrap()
        .build()
        .unwrap();
        let state = AppState {
            db,
            sessions: Default::default(),
            password_attempts: Default::default(),
            privacy: Default::default(),
            balance_writes: Default::default(),
            fund_migration_ready: Default::default(),
            fx_client: reqwest::Client::new(),
            fx_fetches: Default::default(),
            market_client: reqwest::Client::new(),
            market_fetches: Default::default(),
            sms_api_token_hash: Default::default(),
            pending_sms: Default::default(),
            webauthn: std::sync::Arc::new(webauthn),
            passkey_registrations: Default::default(),
            passkey_enrollments: Default::default(),
            passkey_authentications: Default::default(),
            passkey_repairs: Default::default(),
        };
        let dek = crypto::Dek::new([42; crypto::DEK_LEN]);
        let mut ids = Vec::new();
        for (kind, parent_id, offset) in [
            ("investment", None, 0),
            ("bank", None, balance),
            ("investment_fund", Some(1), 0),
        ] {
            let model = account::ActiveModel {
                name: Set(crypto::encrypt(&dek, kind.as_bytes())),
                kind: Set(kind.into()),
                currency: Set("CNY".into()),
                parent_id: Set(parent_id),
                balance_offset: Set(crypto::encrypt_cents(&dek, offset)),
                transfer_fee_rate_bps: Set(crypto::encrypt_cents(&dek, 0)),
                sms_names: Set(crypto::encrypt(&dek, b"[]")),
                note: Set(crypto::encrypt(&dek, b"")),
                created_at: Set(chrono::Utc::now()),
                ..Default::default()
            }
            .insert(&state.db)
            .await
            .unwrap();
            ids.push(model.id);
        }
        Self {
            state,
            dek,
            source: ids[1],
            fund: ids[2],
        }
    }

    async fn plan(&self, strategy: &str) -> recurring_investment::Model {
        recurring_investment::ActiveModel {
            name: Set(crypto::encrypt(&self.dek, b"manual date regression")),
            from_account_id: Set(self.source),
            fund_account_id: Set(self.fund),
            amount: Set(crypto::encrypt_cents(&self.dek, 1_000)),
            fee_rate_bps: Set(crypto::encrypt_cents(&self.dek, 100)),
            strategy: Set(strategy.into()),
            index_code: Set(String::new()),
            moving_average_days: Set(180),
            start_date: Set(day("2020-01-01")),
            next_trade_date: Set(day("2020-01-02")),
            active: Set(true),
            note: Set(crypto::encrypt(&self.dek, b"")),
            created_at: Set(chrono::Utc::now()),
            ..Default::default()
        }
        .insert(&self.state.db)
        .await
        .unwrap()
    }

    async fn confirm(
        &self,
        id: i64,
        submitted: &str,
        amount: &str,
        today: &str,
    ) -> HandlerResult<()> {
        let _guard = self.state.balance_writes.lock().await;
        confirm_manual_on(
            &self.state,
            &self.dek,
            id,
            ManualExecuteFormData {
                trade_date: submitted.into(),
                amount: amount.into(),
            },
            day(today),
        )
        .await
    }

    async fn counts(&self) -> (u64, u64, u64) {
        (
            investment_execution::Entity::find()
                .count(&self.state.db)
                .await
                .unwrap(),
            transfer::Entity::find()
                .count(&self.state.db)
                .await
                .unwrap(),
            bill::Entity::find().count(&self.state.db).await.unwrap(),
        )
    }

    async fn balance(&self, id: i64) -> i64 {
        crate::handlers::accounts::current_balance(&self.state, &self.dek, id)
            .await
            .unwrap()
    }

    async fn stored_plan(&self, id: i64) -> recurring_investment::Model {
        recurring_investment::Entity::find_by_id(id)
            .one(&self.state.db)
            .await
            .unwrap()
            .unwrap()
    }

    async fn sql(&self, sql: &str) {
        self.state
            .db
            .execute(Statement::from_string(DbBackend::Sqlite, sql.to_owned()))
            .await
            .unwrap();
    }

    fn form(&self, strategy: &str, start: NaiveDate) -> InvestmentFormData {
        InvestmentFormData {
            name: "edited plan".into(),
            amount: if strategy == "manual" { "0" } else { "10" }.into(),
            fee_rate: "1".into(),
            strategy: strategy.into(),
            index_code: String::new(),
            moving_average_days: "180".into(),
            from_account_id: self.source,
            fund_account_id: self.fund,
            start_date: start.to_string(),
            note: String::new(),
            active: Some("true".into()),
        }
    }
}

fn day(value: &str) -> NaiveDate {
    NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap()
}

#[tokio::test]
async fn editing_manual_plan_discards_backlog_but_preserves_future_start_and_other_strategies() {
    let app = Fixture::new(100_000).await;
    let plan = app.plan("fixed").await;
    let before = china_today();
    update(
        State(app.state.clone()),
        Extension(SessionDek(app.dek.clone())),
        Path(plan.id),
        Form(app.form("manual", plan.start_date)),
    )
    .await
    .unwrap();
    let after = china_today();
    let changed = app.stored_plan(plan.id).await;
    assert!(changed.next_trade_date >= next_trading_day(&app.state, before).await.unwrap());
    assert!(changed.next_trade_date <= next_trading_day(&app.state, after).await.unwrap());
    assert_eq!(crypto::decrypt_cents(&app.dek, &changed.amount), 0);
    assert_eq!(app.counts().await, (0, 0, 0));

    let fixed = app.plan("fixed").await;
    update(
        State(app.state.clone()),
        Extension(SessionDek(app.dek.clone())),
        Path(fixed.id),
        Form(app.form("fixed", fixed.start_date)),
    )
    .await
    .unwrap();
    assert_eq!(
        app.stored_plan(fixed.id).await.next_trade_date,
        fixed.next_trade_date
    );

    let future_start = after + chrono::Duration::days(30);
    update(
        State(app.state.clone()),
        Extension(SessionDek(app.dek.clone())),
        Path(plan.id),
        Form(app.form("manual", future_start)),
    )
    .await
    .unwrap();
    assert_eq!(
        app.stored_plan(plan.id).await.next_trade_date,
        next_trading_day(&app.state, future_start).await.unwrap()
    );
}

#[tokio::test]
async fn manual_confirmation_only_moves_todays_money_and_cannot_replay_or_backdate() {
    let app = Fixture::new(100_000).await;
    let plan = app.plan("manual").await;
    for (submitted, amount) in [
        ("2020-01-02", "10"),
        ("2026-05-12", "10"),
        ("2026-05-11", "-1"),
        ("2026-05-11", "-0.001"),
        ("2026-05-11", "not-money"),
        ("2026-05-11", "92233720368547758.08"),
    ] {
        assert_eq!(
            app.confirm(plan.id, submitted, amount, "2026-05-11")
                .await
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(app.counts().await, (0, 0, 0));
        assert_eq!(
            app.stored_plan(plan.id).await.next_trade_date,
            plan.next_trade_date
        );
    }
    app.confirm(plan.id, "2026-05-11", "60", "2026-05-11")
        .await
        .unwrap();
    assert_eq!(app.counts().await, (1, 1, 1));
    assert_eq!(app.balance(app.source).await, 93_940);
    assert_eq!(app.balance(app.fund).await, 6_000);
    let execution = investment_execution::Entity::find()
        .one(&app.state.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(execution.trade_date, day("2026-05-11"));
    assert_eq!(
        app.stored_plan(plan.id).await.next_trade_date,
        day("2026-05-12")
    );
    assert_eq!(
        app.confirm(plan.id, "2026-05-11", "0", "2026-05-11")
            .await
            .unwrap_err()
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(app.counts().await, (1, 1, 1));
    assert_eq!(app.balance(app.source).await, 93_940);
}

#[tokio::test]
async fn zero_confirmation_is_atomic_and_records_no_transfer_or_fee_even_without_balance() {
    let app = Fixture::new(0).await;
    let plan = app.plan("manual").await;
    app.sql("CREATE TRIGGER reject_confirmation BEFORE INSERT ON investment_executions BEGIN SELECT RAISE(ABORT, 'isolated write failure'); END").await;
    assert_eq!(
        app.confirm(plan.id, "2026-05-11", "0", "2026-05-11")
            .await
            .unwrap_err()
            .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(app.counts().await, (0, 0, 0));
    assert_eq!(
        app.stored_plan(plan.id).await.next_trade_date,
        plan.next_trade_date
    );
    app.sql("DROP TRIGGER reject_confirmation").await;
    app.confirm(plan.id, "2026-05-11", "0", "2026-05-11")
        .await
        .unwrap();
    assert_eq!(app.counts().await, (1, 0, 0));
    assert_eq!(app.balance(app.source).await, 0);
    assert_eq!(app.balance(app.fund).await, 0);
    let execution = investment_execution::Entity::find()
        .one(&app.state.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(execution.trade_date, day("2026-05-11"));
    assert_eq!(execution.transfer_id, None);
    assert_eq!(execution.fee_bill_id, None);
    assert_eq!(crypto::decrypt_cents(&app.dek, &execution.base_amount), 0);
    assert_eq!(
        app.stored_plan(plan.id).await.next_trade_date,
        day("2026-05-12")
    );
    assert_eq!(
        app.confirm(plan.id, "2026-05-11", "20", "2026-05-11")
            .await
            .unwrap_err()
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(app.counts().await, (1, 0, 0));
}

#[tokio::test]
async fn manual_confirmation_cannot_skip_future_schedule_or_closed_market_days() {
    let app = Fixture::new(0).await;
    let plan = app.plan("manual").await;
    let mut future = plan.clone().into_active_model();
    future.next_trade_date = Set(day("2026-05-12"));
    future.update(&app.state.db).await.unwrap();
    assert_eq!(
        app.confirm(plan.id, "2026-05-11", "0", "2026-05-11")
            .await
            .unwrap_err()
            .0,
        StatusCode::BAD_REQUEST
    );
    let past = app.plan("manual").await;
    assert_eq!(
        app.confirm(past.id, "2026-05-16", "0", "2026-05-16")
            .await
            .unwrap_err()
            .0,
        StatusCode::BAD_REQUEST
    );
    app.sql("INSERT INTO market_closed_days (date, name, source, created_at) VALUES ('2026-05-11', 'isolated closure', 'user', '2026-05-01 00:00:00+00:00')").await;
    assert_eq!(
        app.confirm(past.id, "2026-05-11", "0", "2026-05-11")
            .await
            .unwrap_err()
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(app.counts().await, (0, 0, 0));
    assert_eq!(
        app.stored_plan(past.id).await.next_trade_date,
        past.next_trade_date
    );
}
