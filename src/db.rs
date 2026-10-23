use anyhow::Result;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::SqlitePool;

/// A mirrored channel pairing.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChannelPair {
    pub slack_channel_id: String,
    pub slack_channel_name: String,
    pub discord_channel_id: String,
    pub discord_webhook_id: String,
    pub discord_webhook_token: String,
    pub is_private: i64,
    pub archived: i64,
}

/// A mirrored message's Discord side, for edits and deletes.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MessageRow {
    pub discord_msg_id: String,
    pub origin: String,
    pub discord_thread_id: Option<String>,
}

#[derive(Clone)]
pub struct Db {
    pub pool: SqlitePool,
}

impl Db {
    pub async fn connect(url: &str) -> Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(url)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    // ── channel pairs ──────────────────────────────────────────────────────

    pub async fn pair_by_slack(&self, slack_channel_id: &str) -> Result<Option<ChannelPair>> {
        Ok(sqlx::query_as::<_, ChannelPair>(
            "SELECT * FROM channel_pairs WHERE slack_channel_id = ?",
        )
        .bind(slack_channel_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn pair_by_discord(&self, discord_channel_id: &str) -> Result<Option<ChannelPair>> {
        Ok(sqlx::query_as::<_, ChannelPair>(
            "SELECT * FROM channel_pairs WHERE discord_channel_id = ?",
        )
        .bind(discord_channel_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn upsert_pair(&self, p: &ChannelPair) -> Result<()> {
        sqlx::query(
            "INSERT INTO channel_pairs
                (slack_channel_id, slack_channel_name, discord_channel_id,
                 discord_webhook_id, discord_webhook_token, is_private, archived)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(slack_channel_id) DO UPDATE SET
                slack_channel_name = excluded.slack_channel_name,
                discord_channel_id = excluded.discord_channel_id,
                discord_webhook_id = excluded.discord_webhook_id,
                discord_webhook_token = excluded.discord_webhook_token,
                is_private = excluded.is_private,
                archived = excluded.archived",
        )
        .bind(&p.slack_channel_id)
        .bind(&p.slack_channel_name)
        .bind(&p.discord_channel_id)
        .bind(&p.discord_webhook_id)
        .bind(&p.discord_webhook_token)
        .bind(p.is_private)
        .bind(p.archived)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn set_archived(&self, slack_channel_id: &str, archived: bool) -> Result<()> {
        sqlx::query("UPDATE channel_pairs SET archived = ? WHERE slack_channel_id = ?")
            .bind(archived as i64)
            .bind(slack_channel_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn rename_pair(&self, slack_channel_id: &str, new_name: &str) -> Result<()> {
        sqlx::query("UPDATE channel_pairs SET slack_channel_name = ? WHERE slack_channel_id = ?")
            .bind(new_name)
            .bind(slack_channel_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ── message mapping ──────────────────────────────────────────────────────

    pub async fn map_message(
        &self,
        slack_channel_id: &str,
        slack_ts: &str,
        discord_msg_id: &str,
        origin: &str,
        slack_thread_ts: Option<&str>,
        discord_thread_id: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO messages
                (slack_channel_id, slack_ts, discord_msg_id, origin,
                 slack_thread_ts, discord_thread_id)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(slack_channel_id)
        .bind(slack_ts)
        .bind(discord_msg_id)
        .bind(origin)
        .bind(slack_thread_ts)
        .bind(discord_thread_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn message_by_slack_ts(
        &self,
        slack_channel_id: &str,
        slack_ts: &str,
    ) -> Result<Option<MessageRow>> {
        Ok(sqlx::query_as::<_, MessageRow>(
            "SELECT discord_msg_id, origin, discord_thread_id FROM messages
             WHERE slack_channel_id = ? AND slack_ts = ?",
        )
        .bind(slack_channel_id)
        .bind(slack_ts)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn delete_message_mapping(&self, slack_channel_id: &str, slack_ts: &str) -> Result<()> {
        sqlx::query("DELETE FROM messages WHERE slack_channel_id = ? AND slack_ts = ?")
            .bind(slack_channel_id)
            .bind(slack_ts)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The Slack thread a Discord message belongs to: its thread root if it is
    /// a reply, else its own ts (so replying to it starts a thread there).
    pub async fn slack_thread_root_for_discord_msg(&self, discord_msg_id: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar::<_, String>(
            "SELECT COALESCE(slack_thread_ts, slack_ts) FROM messages
             WHERE discord_msg_id = ? AND slack_ts != ''",
        )
        .bind(discord_msg_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// (slack channel, slack ts, origin) for a mirrored Discord message.
    pub async fn slack_ref_for_discord_msg(
        &self,
        discord_msg_id: &str,
    ) -> Result<Option<(String, String, String)>> {
        Ok(sqlx::query_as::<_, (String, String, String)>(
            "SELECT slack_channel_id, slack_ts, origin FROM messages WHERE discord_msg_id = ?",
        )
        .bind(discord_msg_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn delete_mapping_by_discord(&self, discord_msg_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM messages WHERE discord_msg_id = ?")
            .bind(discord_msg_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Forget a channel pairing and everything mapped under it (used when the
    /// Discord channel was deleted and must be recreated).
    pub async fn reset_channel(&self, slack_channel_id: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for table in ["messages", "threads", "channel_topics", "channel_pairs"] {
            sqlx::query(&format!("DELETE FROM {table} WHERE slack_channel_id = ?"))
                .bind(slack_channel_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn stored_topic(&self, slack_channel_id: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar::<_, String>(
            "SELECT topic FROM channel_topics WHERE slack_channel_id = ?",
        )
        .bind(slack_channel_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn store_topic(&self, slack_channel_id: &str, topic: &str) -> Result<()> {
        sqlx::query("INSERT OR REPLACE INTO channel_topics (slack_channel_id, topic) VALUES (?, ?)")
            .bind(slack_channel_id)
            .bind(topic)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn discord_id_for_slack_ts(
        &self,
        slack_channel_id: &str,
        slack_ts: &str,
    ) -> Result<Option<String>> {
        Ok(sqlx::query_scalar::<_, String>(
            "SELECT discord_msg_id FROM messages WHERE slack_channel_id = ? AND slack_ts = ?",
        )
        .bind(slack_channel_id)
        .bind(slack_ts)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn slack_ts_for_discord_id(&self, discord_msg_id: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar::<_, String>(
            "SELECT slack_ts FROM messages WHERE discord_msg_id = ?",
        )
        .bind(discord_msg_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    // ── threads ──────────────────────────────────────────────────────

    pub async fn map_thread(
        &self,
        slack_channel_id: &str,
        slack_thread_ts: &str,
        discord_thread_id: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO threads
                (slack_channel_id, slack_thread_ts, discord_thread_id)
             VALUES (?, ?, ?)",
        )
        .bind(slack_channel_id)
        .bind(slack_thread_ts)
        .bind(discord_thread_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn discord_thread_for(
        &self,
        slack_channel_id: &str,
        slack_thread_ts: &str,
    ) -> Result<Option<String>> {
        Ok(sqlx::query_scalar::<_, String>(
            "SELECT discord_thread_id FROM threads WHERE slack_channel_id = ? AND slack_thread_ts = ?",
        )
        .bind(slack_channel_id)
        .bind(slack_thread_ts)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn slack_thread_for_discord(
        &self,
        discord_thread_id: &str,
    ) -> Result<Option<(String, String)>> {
        Ok(sqlx::query_as::<_, (String, String)>(
            "SELECT slack_channel_id, slack_thread_ts FROM threads WHERE discord_thread_id = ?",
        )
        .bind(discord_thread_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    // ── loop prevention ──────────────────────────────────────────────────────

    pub async fn mark_self_posted(&self, slack_ts: &str) -> Result<()> {
        sqlx::query("INSERT OR IGNORE INTO self_posted_slack (slack_ts) VALUES (?)")
            .bind(slack_ts)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn is_self_posted(&self, slack_ts: &str) -> Result<bool> {
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM self_posted_slack WHERE slack_ts = ?")
                .bind(slack_ts)
                .fetch_one(&self.pool)
                .await?;
        Ok(n > 0)
    }

    // ── user aliases ──────────────────────────────────────────────────────

    pub async fn slack_id_for_alias(&self, alias: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar::<_, Option<String>>(
            "SELECT slack_user_id FROM user_aliases WHERE alias = ?",
        )
        .bind(alias)
        .fetch_optional(&self.pool)
        .await?
        .flatten())
    }

    pub async fn discord_id_for_alias(&self, alias: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar::<_, Option<String>>(
            "SELECT discord_user_id FROM user_aliases WHERE alias = ?",
        )
        .bind(alias)
        .fetch_optional(&self.pool)
        .await?
        .flatten())
    }

    /// The Discord account linked to a Slack user through a shared alias.
    pub async fn discord_id_for_slack_user(&self, slack_user_id: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar::<_, String>(
            "SELECT discord_user_id FROM user_aliases
             WHERE slack_user_id = ? AND discord_user_id IS NOT NULL LIMIT 1",
        )
        .bind(slack_user_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn set_discord_alias(&self, alias: &str, discord_user_id: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO user_aliases (alias, discord_user_id) VALUES (?, ?)
             ON CONFLICT(alias) DO UPDATE SET discord_user_id = excluded.discord_user_id",
        )
        .bind(alias)
        .bind(discord_user_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn set_slack_alias(&self, alias: &str, slack_user_id: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO user_aliases (alias, slack_user_id) VALUES (?, ?)
             ON CONFLICT(alias) DO UPDATE SET slack_user_id = excluded.slack_user_id",
        )
        .bind(alias)
        .bind(slack_user_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
