use rusqlite::{params, Result, Row};

use super::{
    ApiKeyModelTokenUsageSummary, ApiKeyTokenUsageSummary, DailyTokenUsageRollup,
    RequestLogTodaySummary, RequestTokenStat, SourceTokenUsageRollup, Storage, TokenUsageRollup,
    TokenUsageSummary, UserTokenUsageRollup,
};

const TOKEN_ROLLUP_COLUMNS: &str = "
    IFNULL(SUM(IFNULL(t.input_tokens, 0)), 0) AS input_tokens,
    IFNULL(SUM(IFNULL(t.cached_input_tokens, 0)), 0) AS cached_input_tokens,
    IFNULL(SUM(IFNULL(t.output_tokens, 0)), 0) AS output_tokens,
    IFNULL(SUM(IFNULL(t.reasoning_output_tokens, 0)), 0) AS reasoning_output_tokens,
    IFNULL(
        SUM(
            CASE
                WHEN t.total_tokens IS NOT NULL THEN
                    CASE WHEN t.total_tokens > 0 THEN t.total_tokens ELSE 0 END
                ELSE
                    CASE
                        WHEN IFNULL(t.input_tokens, 0) - IFNULL(t.cached_input_tokens, 0) + IFNULL(t.output_tokens, 0) > 0
                            THEN IFNULL(t.input_tokens, 0) - IFNULL(t.cached_input_tokens, 0) + IFNULL(t.output_tokens, 0)
                        ELSE 0
                    END
            END
        ),
        0
    ) AS total_tokens,
    IFNULL(SUM(IFNULL(t.estimated_cost_usd, 0.0)), 0.0) AS estimated_cost_usd,
    COUNT(DISTINCT r.id) AS request_count,
    COUNT(DISTINCT CASE WHEN r.status_code >= 200 AND r.status_code <= 299 THEN r.id END) AS success_count,
    COUNT(DISTINCT CASE WHEN IFNULL(r.status_code, 0) >= 400 OR TRIM(IFNULL(r.error, '')) <> '' THEN r.id END) AS error_count";

const USER_OWNER_EXPR: &str =
    "COALESCE(NULLIF(TRIM(charge.owner_id), ''), NULLIF(TRIM(owner.owner_user_id), ''))";

// User attribution prefers the request_charge wallet owner. The api_key_owners
// fallback is current-owner based, so old uncharged logs are approximate.
const USER_OWNER_JOINS: &str = "
    LEFT JOIN (
        SELECT l.request_log_id, MIN(w.owner_id) AS owner_id
        FROM app_wallet_ledger_entries l
        JOIN app_wallets w ON w.id = l.wallet_id
        WHERE l.entry_kind = 'request_charge'
          AND w.owner_kind = 'user'
        GROUP BY l.request_log_id
    ) charge ON charge.request_log_id = r.id
    LEFT JOIN api_key_owners owner ON owner.key_id = r.key_id AND owner.owner_kind = 'user'";

fn token_usage_rollup_from_row(row: &Row<'_>, offset: usize) -> Result<TokenUsageRollup> {
    Ok(TokenUsageRollup {
        input_tokens: row.get::<_, i64>(offset)?.max(0),
        cached_input_tokens: row.get::<_, i64>(offset + 1)?.max(0),
        output_tokens: row.get::<_, i64>(offset + 2)?.max(0),
        reasoning_output_tokens: row.get::<_, i64>(offset + 3)?.max(0),
        total_tokens: row.get::<_, i64>(offset + 4)?.max(0),
        estimated_cost_usd: row.get::<_, f64>(offset + 5)?.max(0.0),
        request_count: row.get::<_, i64>(offset + 6)?.max(0),
        success_count: row.get::<_, i64>(offset + 7)?.max(0),
        error_count: row.get::<_, i64>(offset + 8)?.max(0),
    })
}

fn source_id_expr(source_kind: &str) -> Option<&'static str> {
    match source_kind {
        "openai_account" => Some(
            // Prefer actual_source_* written by routing. Legacy account_id is only
            // used when actual source metadata was not captured.
            "CASE
                WHEN r.actual_source_kind = 'openai_account'
                    THEN COALESCE(NULLIF(TRIM(r.actual_source_id), ''), NULLIF(TRIM(r.account_id), ''))
                WHEN r.actual_source_kind IS NULL OR TRIM(r.actual_source_kind) = ''
                    THEN NULLIF(TRIM(r.account_id), '')
                ELSE NULL
             END",
        ),
        "aggregate_api" => Some(
            // Prefer actual_source_* written by routing. Legacy aggregate API
            // context is only used when actual source metadata was not captured.
            "CASE
                WHEN r.actual_source_kind = 'aggregate_api'
                    THEN COALESCE(NULLIF(TRIM(r.actual_source_id), ''), NULLIF(TRIM(r.initial_aggregate_api_id), ''))
                WHEN r.actual_source_kind IS NULL OR TRIM(r.actual_source_kind) = ''
                    THEN NULLIF(TRIM(r.initial_aggregate_api_id), '')
                ELSE NULL
             END",
        ),
        _ => None,
    }
}

impl Storage {
    /// 函数 `insert_request_token_stat`
    ///
    /// 作者: gaohongshun
    ///
    /// 时间: 2026-04-02
    ///
    /// # 参数
    /// - self: 参数 self
    /// - stat: 参数 stat
    ///
    /// # 返回
    /// 返回函数执行结果
    pub fn insert_request_token_stat(&self, stat: &RequestTokenStat) -> Result<()> {
        self.conn.execute(
            "INSERT INTO request_token_stats (
                request_log_id, key_id, account_id, model,
                input_tokens, cached_input_tokens, output_tokens, total_tokens, reasoning_output_tokens,
                estimated_cost_usd, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            (
                stat.request_log_id,
                &stat.key_id,
                &stat.account_id,
                &stat.model,
                stat.input_tokens,
                stat.cached_input_tokens,
                stat.output_tokens,
                stat.total_tokens,
                stat.reasoning_output_tokens,
                stat.estimated_cost_usd,
                stat.created_at,
            ),
        )?;
        Ok(())
    }

    /// 函数 `summarize_request_token_stats_between`
    ///
    /// 作者: gaohongshun
    ///
    /// 时间: 2026-04-02
    ///
    /// # 参数
    /// - self: 参数 self
    /// - start_ts: 参数 start_ts
    /// - end_ts: 参数 end_ts
    ///
    /// # 返回
    /// 返回函数执行结果
    pub fn summarize_request_token_stats_between(
        &self,
        start_ts: i64,
        end_ts: i64,
    ) -> Result<RequestLogTodaySummary> {
        let mut stmt = self.conn.prepare(
            "SELECT
                IFNULL(SUM(input_tokens), 0),
                IFNULL(SUM(cached_input_tokens), 0),
                IFNULL(SUM(output_tokens), 0),
                IFNULL(SUM(reasoning_output_tokens), 0),
                IFNULL(SUM(estimated_cost_usd), 0.0)
             FROM request_token_stats
             WHERE created_at >= ?1 AND created_at < ?2",
        )?;
        let mut rows = stmt.query((start_ts, end_ts))?;
        if let Some(row) = rows.next()? {
            return Ok(RequestLogTodaySummary {
                input_tokens: row.get(0)?,
                cached_input_tokens: row.get(1)?,
                output_tokens: row.get(2)?,
                reasoning_output_tokens: row.get(3)?,
                estimated_cost_usd: row.get(4)?,
            });
        }
        Ok(RequestLogTodaySummary {
            input_tokens: 0,
            cached_input_tokens: 0,
            output_tokens: 0,
            reasoning_output_tokens: 0,
            estimated_cost_usd: 0.0,
        })
    }

    /// 函数 `summarize_request_token_stats_by_key`
    ///
    /// 作者: gaohongshun
    ///
    /// 时间: 2026-04-02
    ///
    /// # 参数
    /// - self: 参数 self
    ///
    /// # 返回
    /// 返回函数执行结果
    pub fn summarize_request_token_stats_by_key(&self) -> Result<Vec<ApiKeyTokenUsageSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT
                key_id,
                IFNULL(
                    SUM(
                        CASE
                            WHEN total_tokens IS NOT NULL THEN
                                CASE WHEN total_tokens > 0 THEN total_tokens ELSE 0 END
                            ELSE
                                CASE
                                    WHEN IFNULL(input_tokens, 0) - IFNULL(cached_input_tokens, 0) + IFNULL(output_tokens, 0) > 0
                                        THEN IFNULL(input_tokens, 0) - IFNULL(cached_input_tokens, 0) + IFNULL(output_tokens, 0)
                                    ELSE 0
                                END
                        END
                    ),
                    0
                ) AS total_tokens,
                IFNULL(SUM(estimated_cost_usd), 0.0) AS estimated_cost_usd
             FROM request_token_stats
             WHERE key_id IS NOT NULL AND TRIM(key_id) <> ''
             GROUP BY key_id
             ORDER BY total_tokens DESC, key_id ASC",
        )?;
        let mut rows = stmt.query([])?;
        let mut items = Vec::new();
        while let Some(row) = rows.next()? {
            items.push(ApiKeyTokenUsageSummary {
                key_id: row.get(0)?,
                total_tokens: row.get(1)?,
                estimated_cost_usd: row.get(2)?,
            });
        }
        Ok(items)
    }

    pub fn summarize_request_token_stats_by_model(
        &self,
        start_ts: Option<i64>,
        end_ts: Option<i64>,
    ) -> Result<Vec<TokenUsageSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT
                COALESCE(NULLIF(TRIM(model), ''), 'unknown') AS normalized_model,
                IFNULL(SUM(input_tokens), 0) AS input_tokens,
                IFNULL(SUM(cached_input_tokens), 0) AS cached_input_tokens,
                IFNULL(SUM(output_tokens), 0) AS output_tokens,
                IFNULL(SUM(reasoning_output_tokens), 0) AS reasoning_output_tokens,
                IFNULL(
                    SUM(
                        CASE
                            WHEN total_tokens IS NOT NULL THEN
                                CASE WHEN total_tokens > 0 THEN total_tokens ELSE 0 END
                            ELSE
                                CASE
                                    WHEN IFNULL(input_tokens, 0) - IFNULL(cached_input_tokens, 0) + IFNULL(output_tokens, 0) > 0
                                        THEN IFNULL(input_tokens, 0) - IFNULL(cached_input_tokens, 0) + IFNULL(output_tokens, 0)
                                    ELSE 0
                                END
                        END
                    ),
                    0
                ) AS total_tokens,
                IFNULL(SUM(estimated_cost_usd), 0.0) AS estimated_cost_usd
             FROM request_token_stats
             WHERE (?1 IS NULL OR created_at >= ?1)
               AND (?2 IS NULL OR created_at < ?2)
             GROUP BY normalized_model
             ORDER BY total_tokens DESC, normalized_model ASC",
        )?;
        let mut rows = stmt.query((start_ts, end_ts))?;
        let mut items = Vec::new();
        while let Some(row) = rows.next()? {
            items.push(TokenUsageSummary {
                model: row.get(0)?,
                input_tokens: row.get::<_, i64>(1)?.max(0),
                cached_input_tokens: row.get::<_, i64>(2)?.max(0),
                output_tokens: row.get::<_, i64>(3)?.max(0),
                reasoning_output_tokens: row.get::<_, i64>(4)?.max(0),
                total_tokens: row.get::<_, i64>(5)?.max(0),
                estimated_cost_usd: row.get::<_, f64>(6)?.max(0.0),
            });
        }
        Ok(items)
    }

    pub fn summarize_request_token_stats_by_key_and_model(
        &self,
        start_ts: Option<i64>,
        end_ts: Option<i64>,
    ) -> Result<Vec<ApiKeyModelTokenUsageSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT
                key_id,
                COALESCE(NULLIF(TRIM(model), ''), 'unknown') AS normalized_model,
                IFNULL(SUM(input_tokens), 0) AS input_tokens,
                IFNULL(SUM(cached_input_tokens), 0) AS cached_input_tokens,
                IFNULL(SUM(output_tokens), 0) AS output_tokens,
                IFNULL(SUM(reasoning_output_tokens), 0) AS reasoning_output_tokens,
                IFNULL(
                    SUM(
                        CASE
                            WHEN total_tokens IS NOT NULL THEN
                                CASE WHEN total_tokens > 0 THEN total_tokens ELSE 0 END
                            ELSE
                                CASE
                                    WHEN IFNULL(input_tokens, 0) - IFNULL(cached_input_tokens, 0) + IFNULL(output_tokens, 0) > 0
                                        THEN IFNULL(input_tokens, 0) - IFNULL(cached_input_tokens, 0) + IFNULL(output_tokens, 0)
                                    ELSE 0
                                END
                        END
                    ),
                    0
                ) AS total_tokens,
                IFNULL(SUM(estimated_cost_usd), 0.0) AS estimated_cost_usd
             FROM request_token_stats
             WHERE key_id IS NOT NULL AND TRIM(key_id) <> ''
               AND (?1 IS NULL OR created_at >= ?1)
               AND (?2 IS NULL OR created_at < ?2)
             GROUP BY key_id, normalized_model
             ORDER BY total_tokens DESC, key_id ASC, normalized_model ASC",
        )?;
        let mut rows = stmt.query((start_ts, end_ts))?;
        let mut items = Vec::new();
        while let Some(row) = rows.next()? {
            items.push(ApiKeyModelTokenUsageSummary {
                key_id: row.get(0)?,
                model: row.get(1)?,
                input_tokens: row.get::<_, i64>(2)?.max(0),
                cached_input_tokens: row.get::<_, i64>(3)?.max(0),
                output_tokens: row.get::<_, i64>(4)?.max(0),
                reasoning_output_tokens: row.get::<_, i64>(5)?.max(0),
                total_tokens: row.get::<_, i64>(6)?.max(0),
                estimated_cost_usd: row.get::<_, f64>(7)?.max(0.0),
            });
        }
        Ok(items)
    }

    pub fn summarize_request_token_stats_daily(
        &self,
        start_ts: i64,
        end_ts: i64,
        bucket_seconds: i64,
    ) -> Result<Vec<DailyTokenUsageRollup>> {
        if end_ts <= start_ts {
            return Ok(Vec::new());
        }
        let bucket_seconds = bucket_seconds.max(1);
        let sql = format!(
            "SELECT
                ?1 + CAST((r.created_at - ?1) / ?3 AS INTEGER) * ?3 AS bucket_start,
                MIN(?1 + (CAST((r.created_at - ?1) / ?3 AS INTEGER) + 1) * ?3, ?2) AS bucket_end,
                {TOKEN_ROLLUP_COLUMNS}
             FROM request_logs r
             LEFT JOIN request_token_stats t ON t.request_log_id = r.id
             WHERE r.created_at >= ?1 AND r.created_at < ?2
             GROUP BY bucket_start
             ORDER BY bucket_start ASC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(params![start_ts, end_ts, bucket_seconds])?;
        let mut items = Vec::new();
        while let Some(row) = rows.next()? {
            items.push(DailyTokenUsageRollup {
                day_start_ts: row.get(0)?,
                day_end_ts: row.get(1)?,
                usage: token_usage_rollup_from_row(row, 2)?,
            });
        }
        Ok(items)
    }

    pub fn summarize_request_token_stats_by_user_between(
        &self,
        start_ts: i64,
        end_ts: i64,
    ) -> Result<Vec<UserTokenUsageRollup>> {
        if end_ts <= start_ts {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT
                {USER_OWNER_EXPR} AS user_id,
                {TOKEN_ROLLUP_COLUMNS}
             FROM request_logs r
             LEFT JOIN request_token_stats t ON t.request_log_id = r.id
             {USER_OWNER_JOINS}
             WHERE r.created_at >= ?1 AND r.created_at < ?2
               AND {USER_OWNER_EXPR} IS NOT NULL
             GROUP BY user_id
             ORDER BY total_tokens DESC, user_id ASC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(params![start_ts, end_ts])?;
        let mut items = Vec::new();
        while let Some(row) = rows.next()? {
            items.push(UserTokenUsageRollup {
                user_id: row.get(0)?,
                usage: token_usage_rollup_from_row(row, 1)?,
            });
        }
        Ok(items)
    }

    pub fn summarize_request_token_stats_for_user_between(
        &self,
        user_id: &str,
        start_ts: i64,
        end_ts: i64,
    ) -> Result<TokenUsageRollup> {
        if end_ts <= start_ts || user_id.trim().is_empty() {
            return Ok(TokenUsageRollup::default());
        }
        let sql = format!(
            "SELECT
                {TOKEN_ROLLUP_COLUMNS}
             FROM request_logs r
             LEFT JOIN request_token_stats t ON t.request_log_id = r.id
             {USER_OWNER_JOINS}
             WHERE r.created_at >= ?1 AND r.created_at < ?2
               AND {USER_OWNER_EXPR} = ?3"
        );
        self.conn
            .query_row(&sql, params![start_ts, end_ts, user_id.trim()], |row| {
                token_usage_rollup_from_row(row, 0)
            })
    }

    pub fn summarize_request_token_stats_daily_for_user(
        &self,
        user_id: &str,
        start_ts: i64,
        end_ts: i64,
        bucket_seconds: i64,
    ) -> Result<Vec<DailyTokenUsageRollup>> {
        if end_ts <= start_ts || user_id.trim().is_empty() {
            return Ok(Vec::new());
        }
        let bucket_seconds = bucket_seconds.max(1);
        let sql = format!(
            "SELECT
                ?1 + CAST((r.created_at - ?1) / ?3 AS INTEGER) * ?3 AS bucket_start,
                MIN(?1 + (CAST((r.created_at - ?1) / ?3 AS INTEGER) + 1) * ?3, ?2) AS bucket_end,
                {TOKEN_ROLLUP_COLUMNS}
             FROM request_logs r
             LEFT JOIN request_token_stats t ON t.request_log_id = r.id
             {USER_OWNER_JOINS}
             WHERE r.created_at >= ?1 AND r.created_at < ?2
               AND {USER_OWNER_EXPR} = ?4
             GROUP BY bucket_start
             ORDER BY bucket_start ASC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(params![start_ts, end_ts, bucket_seconds, user_id.trim()])?;
        let mut items = Vec::new();
        while let Some(row) = rows.next()? {
            items.push(DailyTokenUsageRollup {
                day_start_ts: row.get(0)?,
                day_end_ts: row.get(1)?,
                usage: token_usage_rollup_from_row(row, 2)?,
            });
        }
        Ok(items)
    }

    pub fn summarize_request_token_stats_by_source_between(
        &self,
        source_kind: &str,
        start_ts: i64,
        end_ts: i64,
    ) -> Result<Vec<SourceTokenUsageRollup>> {
        if end_ts <= start_ts {
            return Ok(Vec::new());
        }
        let Some(source_id_expr) = source_id_expr(source_kind) else {
            return Ok(Vec::new());
        };
        let sql = format!(
            "SELECT
                {source_id_expr} AS source_id,
                {TOKEN_ROLLUP_COLUMNS}
             FROM request_logs r
             LEFT JOIN request_token_stats t ON t.request_log_id = r.id
             WHERE r.created_at >= ?1 AND r.created_at < ?2
               AND {source_id_expr} IS NOT NULL
             GROUP BY source_id
             ORDER BY total_tokens DESC, source_id ASC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(params![start_ts, end_ts])?;
        let mut items = Vec::new();
        while let Some(row) = rows.next()? {
            items.push(SourceTokenUsageRollup {
                source_kind: source_kind.to_string(),
                source_id: row.get(0)?,
                usage: token_usage_rollup_from_row(row, 1)?,
            });
        }
        Ok(items)
    }

    /// 函数 `ensure_request_token_stats_table`
    ///
    /// 作者: gaohongshun
    ///
    /// 时间: 2026-04-02
    ///
    /// # 参数
    /// - super: 参数 super
    ///
    /// # 返回
    /// 返回函数执行结果
    pub(super) fn ensure_request_token_stats_table(&self) -> Result<()> {
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS request_token_stats (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                request_log_id INTEGER NOT NULL,
                key_id TEXT,
                account_id TEXT,
                model TEXT,
                input_tokens INTEGER,
                cached_input_tokens INTEGER,
                output_tokens INTEGER,
                total_tokens INTEGER,
                reasoning_output_tokens INTEGER,
                estimated_cost_usd REAL,
                created_at INTEGER NOT NULL
            )",
            [],
        )?;
        self.conn.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_request_token_stats_request_log_id
             ON request_token_stats(request_log_id)",
            [],
        )?;
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_request_token_stats_created_at
             ON request_token_stats(created_at DESC)",
            [],
        )?;
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_request_token_stats_account_id_created_at
             ON request_token_stats(account_id, created_at DESC)",
            [],
        )?;
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_request_token_stats_key_id_created_at
             ON request_token_stats(key_id, created_at DESC)",
            [],
        )?;
        self.ensure_column("request_token_stats", "total_tokens", "INTEGER")?;

        if self.has_column("request_logs", "input_tokens")? {
            // 中文注释：迁移历史 request_logs 里的 token 字段，避免升级后今日统计突然归零。
            self.conn.execute(
                "INSERT OR IGNORE INTO request_token_stats (
                    request_log_id, key_id, account_id, model,
                    input_tokens, cached_input_tokens, output_tokens, total_tokens, reasoning_output_tokens,
                    estimated_cost_usd, created_at
                 )
                 SELECT
                    id, key_id, account_id, model,
                    input_tokens, cached_input_tokens, output_tokens, NULL, reasoning_output_tokens,
                    estimated_cost_usd, created_at
                 FROM request_logs
                 WHERE input_tokens IS NOT NULL
                    OR cached_input_tokens IS NOT NULL
                    OR output_tokens IS NOT NULL
                    OR reasoning_output_tokens IS NOT NULL
                    OR estimated_cost_usd IS NOT NULL",
                [],
            )?;
        }
        Ok(())
    }
}
