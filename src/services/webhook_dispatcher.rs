//! Webhook dispatcher with per-endpoint reliability tracking.
//!
//! After every delivery attempt the dispatcher:
//! 1. Inserts a row into `webhook_delivery_events`.
//! 2. Recomputes the endpoint's `success_rate` and `total_deliveries` from
//!    the last 100 deliveries.
//! 3. Auto-disables endpoints whose success rate drops below 10 % and records
//!    a notification row.

use crate::error::AppError;
use chrono::Utc;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::time::{Duration, Instant};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Snapshot of an endpoint's health as returned by the admin API.
#[derive(Debug, Serialize, Deserialize)]
pub struct EndpointHealth {
    pub id: Uuid,
    pub url: String,
    pub enabled: bool,
    pub success_rate: f64,
    pub total_deliveries: i32,
    pub last_success_at: Option<chrono::DateTime<Utc>>,
}

/// Threshold below which an endpoint is auto-disabled (10 %).
const AUTO_DISABLE_THRESHOLD: f64 = 10.0;
/// Number of recent deliveries used to compute the rolling success rate.
const ROLLING_WINDOW: i64 = 100;

// ---------------------------------------------------------------------------
// Dispatcher
// ---------------------------------------------------------------------------

pub struct WebhookDispatcher {
    client: Client,
    pool: PgPool,
}

impl WebhookDispatcher {
    pub fn new(pool: PgPool) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("Failed to build HTTP client");
        Self { client, pool }
    }

    /// Dispatch `payload` to `endpoint_url` and record the outcome.
    ///
    /// Returns `Ok(())` on HTTP 2xx, `Err(AppError)` otherwise.
    pub async fn dispatch(
        &self,
        endpoint_id: Uuid,
        endpoint_url: &str,
        payload: &serde_json::Value,
    ) -> Result<(), AppError> {
        let start = Instant::now();

        let response = self
            .client
            .post(endpoint_url)
            .json(payload)
            .send()
            .await;

        let elapsed_ms = start.elapsed().as_millis() as i32;

        let (success, http_status, error_message) = match response {
            Ok(resp) => {
                let status = resp.status().as_u16() as i32;
                let ok = resp.status().is_success();
                let err = if ok {
                    None
                } else {
                    Some(format!("HTTP {}", status))
                };
                (ok, Some(status), err)
            }
            Err(e) => (false, None, Some(e.to_string())),
        };

        // Persist delivery event
        self.record_delivery_event(endpoint_id, success, http_status, elapsed_ms, error_message.as_deref())
            .await?;

        // Recompute reliability stats and potentially auto-disable
        self.update_endpoint_stats(endpoint_id).await?;

        if success {
            Ok(())
        } else {
            Err(AppError::Internal(
                error_message.unwrap_or_else(|| "Webhook delivery failed".to_string()),
            ))
        }
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    async fn record_delivery_event(
        &self,
        endpoint_id: Uuid,
        success: bool,
        http_status: Option<i32>,
        response_time_ms: i32,
        error_message: Option<&str>,
    ) -> Result<(), AppError> {
        sqlx::query(
            r#"
            INSERT INTO webhook_delivery_events
                (endpoint_id, delivered_at, success, http_status, response_time_ms, error_message)
            VALUES ($1, NOW(), $2, $3, $4, $5)
            "#,
        )
        .bind(endpoint_id)
        .bind(success)
        .bind(http_status)
        .bind(response_time_ms)
        .bind(error_message)
        .execute(&self.pool)
        .await
        .map_err(AppError::Database)?;

        Ok(())
    }

    /// Recompute `success_rate` and `total_deliveries` from the last
    /// [`ROLLING_WINDOW`] deliveries, then auto-disable if below threshold.
    async fn update_endpoint_stats(&self, endpoint_id: Uuid) -> Result<(), AppError> {
        // Compute stats from the last N deliveries
        let row = sqlx::query!(
            r#"
            SELECT
                COUNT(*)                                    AS total,
                SUM(CASE WHEN success THEN 1 ELSE 0 END)   AS successes
            FROM (
                SELECT success
                FROM webhook_delivery_events
                WHERE endpoint_id = $1
                ORDER BY delivered_at DESC
                LIMIT $2
            ) recent
            "#,
            endpoint_id,
            ROLLING_WINDOW,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(AppError::Database)?;

        let total = row.total.unwrap_or(0) as i32;
        let successes = row.successes.unwrap_or(0) as f64;
        let success_rate = if total > 0 {
            (successes / total as f64) * 100.0
        } else {
            100.0
        };

        // Update last_success_at if the most recent delivery was successful
        sqlx::query(
            r#"
            UPDATE webhook_endpoints
            SET
                success_rate     = $2,
                total_deliveries = $3,
                last_success_at  = CASE
                    WHEN (
                        SELECT success FROM webhook_delivery_events
                        WHERE endpoint_id = $1
                        ORDER BY delivered_at DESC
                        LIMIT 1
                    ) THEN NOW()
                    ELSE last_success_at
                END,
                updated_at = NOW()
            WHERE id = $1
            "#,
        )
        .bind(endpoint_id)
        .bind(success_rate)
        .bind(total)
        .execute(&self.pool)
        .await
        .map_err(AppError::Database)?;

        // Auto-disable if below threshold and currently enabled
        if success_rate < AUTO_DISABLE_THRESHOLD && total >= 100 {
            self.auto_disable_endpoint(endpoint_id, success_rate).await?;
        }

        Ok(())
    }

    async fn auto_disable_endpoint(
        &self,
        endpoint_id: Uuid,
        success_rate: f64,
    ) -> Result<(), AppError> {
        // Only disable if still enabled (avoid duplicate notifications)
        let updated = sqlx::query!(
            r#"
            UPDATE webhook_endpoints
            SET enabled = FALSE, updated_at = NOW()
            WHERE id = $1 AND enabled = TRUE
            RETURNING id
            "#,
            endpoint_id,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(AppError::Database)?;

        if updated.is_some() {
            tracing::warn!(
                endpoint_id = %endpoint_id,
                success_rate = success_rate,
                "Webhook endpoint auto-disabled due to low success rate"
            );

            // Record notification
            sqlx::query(
                r#"
                INSERT INTO webhook_endpoint_notifications
                    (endpoint_id, reason, success_rate, notified_at)
                VALUES ($1, 'auto_disabled_low_success_rate', $2, NOW())
                "#,
            )
            .bind(endpoint_id)
            .bind(success_rate)
            .execute(&self.pool)
            .await
            .map_err(AppError::Database)?;
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Admin query helpers (used by the admin handler)
// ---------------------------------------------------------------------------

/// Return health scores for all webhook endpoints.
pub async fn list_endpoint_health(pool: &PgPool) -> Result<Vec<EndpointHealth>, AppError> {
    let rows = sqlx::query!(
        r#"
        SELECT id, url, enabled, success_rate, total_deliveries, last_success_at
        FROM webhook_endpoints
        ORDER BY success_rate ASC, total_deliveries DESC
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(AppError::Database)?;

    Ok(rows
        .into_iter()
        .map(|r| EndpointHealth {
            id: r.id,
            url: r.url,
            enabled: r.enabled,
            success_rate: r.success_rate.map(|v| {
                // NUMERIC → f64
                v.to_string().parse::<f64>().unwrap_or(0.0)
            }).unwrap_or(100.0),
            total_deliveries: r.total_deliveries.unwrap_or(0),
            last_success_at: r.last_success_at,
        })
        .collect())
}

/// Return health score for a single endpoint.
pub async fn get_endpoint_health(
    pool: &PgPool,
    endpoint_id: Uuid,
) -> Result<EndpointHealth, AppError> {
    let r = sqlx::query!(
        r#"
        SELECT id, url, enabled, success_rate, total_deliveries, last_success_at
        FROM webhook_endpoints
        WHERE id = $1
        "#,
        endpoint_id,
    )
    .fetch_optional(pool)
    .await
    .map_err(AppError::Database)?
    .ok_or_else(|| AppError::NotFound(format!("Endpoint {} not found", endpoint_id)))?;

    Ok(EndpointHealth {
        id: r.id,
        url: r.url,
        enabled: r.enabled,
        success_rate: r.success_rate.map(|v| {
            v.to_string().parse::<f64>().unwrap_or(0.0)
        }).unwrap_or(100.0),
        total_deliveries: r.total_deliveries.unwrap_or(0),
        last_success_at: r.last_success_at,
    })
}
