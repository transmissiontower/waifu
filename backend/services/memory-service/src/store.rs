use crate::embed::{cosine, from_blob, to_blob};
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Store {
    conn: Connection,
}

pub struct Msg {
    pub id: i64,
    pub role: String,
    pub text: String,
}
pub struct Mem {
    pub text: String,
    pub score: f32,
    pub kind: String,
    pub ts: i64,
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl Store {
    pub fn open(path: &str) -> Result<Self> {
        if path != ":memory:" {
            if let Some(p) = Path::new(path).parent() {
                std::fs::create_dir_all(p).ok();
            }
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS messages(id INTEGER PRIMARY KEY, user_id TEXT NOT NULL, role TEXT NOT NULL, text TEXT NOT NULL, ts INTEGER NOT NULL, summarized INTEGER NOT NULL DEFAULT 0);
             CREATE INDEX IF NOT EXISTS idx_msg_user ON messages(user_id, id);
             CREATE TABLE IF NOT EXISTS memories(id INTEGER PRIMARY KEY, user_id TEXT NOT NULL, message_id INTEGER, kind TEXT NOT NULL, text TEXT NOT NULL, embedder TEXT NOT NULL, embedding BLOB NOT NULL, ts INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS idx_mem_user ON memories(user_id, embedder);
             CREATE TABLE IF NOT EXISTS facts(user_id TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, ts INTEGER NOT NULL, PRIMARY KEY(user_id, key));
             CREATE TABLE IF NOT EXISTS summaries(id INTEGER PRIMARY KEY, user_id TEXT NOT NULL, text TEXT NOT NULL, upto_message_id INTEGER NOT NULL, ts INTEGER NOT NULL);",
        )?;
        Ok(Self { conn })
    }

    pub fn add_message(&self, user: &str, role: &str, text: &str) -> Result<i64> {
        self.conn.execute("INSERT INTO messages(user_id,role,text,ts) VALUES(?,?,?,?)", params![user, role, text, now()])?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn add_memory(&self, user: &str, message_id: Option<i64>, kind: &str, text: &str, embedder: &str, emb: &[f32]) -> Result<()> {
        self.conn.execute(
            "INSERT INTO memories(user_id,message_id,kind,text,embedder,embedding,ts) VALUES(?,?,?,?,?,?,?)",
            params![user, message_id, kind, text, embedder, to_blob(emb), now()],
        )?;
        Ok(())
    }

    pub fn recent(&self, user: &str, n: u32) -> Result<Vec<Msg>> {
        let mut st = self.conn.prepare("SELECT id,role,text FROM messages WHERE user_id=? AND summarized=0 ORDER BY id DESC LIMIT ?")?;
        let mut v: Vec<Msg> =
            st.query_map(params![user, n], |r| Ok(Msg { id: r.get(0)?, role: r.get(1)?, text: r.get(2)? }))?.collect::<Result<_, _>>()?;
        v.reverse();
        Ok(v)
    }

    pub fn search(&self, user: &str, embedder: &str, q: &[f32], k: usize, min_score: f32, exclude_msgs: &[i64]) -> Result<Vec<Mem>> {
        let mut st = self
            .conn
            .prepare("SELECT message_id,kind,text,embedding,ts FROM memories WHERE user_id=? AND embedder=? ORDER BY id DESC LIMIT 5000")?;
        let rows = st.query_map(params![user, embedder], |r| {
            Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Vec<u8>>(3)?, r.get::<_, i64>(4)?))
        })?;
        let mut scored: Vec<Mem> = vec![];
        for row in rows {
            let (mid, kind, text, blob, ts) = row?;
            if mid.map(|m| exclude_msgs.contains(&m)).unwrap_or(false) {
                continue;
            }
            let score = cosine(q, &from_blob(&blob));
            if score >= min_score {
                scored.push(Mem { text, score, kind, ts });
            }
        }
        scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);
        Ok(scored)
    }

    pub fn facts(&self, user: &str) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare("SELECT key,value FROM facts WHERE user_id=? ORDER BY key")?;
        let v = st.query_map(params![user], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
        Ok(v)
    }

    /// likes/dislikes accumulate; everything else is replaced by the newest value.
    pub fn upsert_fact(&self, user: &str, key: &str, value: &str) -> Result<()> {
        let value = if key == "likes" || key == "dislikes" {
            let old: Option<String> =
                self.conn.query_row("SELECT value FROM facts WHERE user_id=? AND key=?", params![user, key], |r| r.get(0)).optional()?;
            match old {
                Some(o) if o.to_lowercase().split(", ").any(|x| x == value.to_lowercase()) => o,
                Some(o) => format!("{o}, {value}"),
                None => value.to_string(),
            }
        } else {
            value.to_string()
        };
        self.conn.execute(
            "INSERT INTO facts(user_id,key,value,ts) VALUES(?,?,?,?) ON CONFLICT(user_id,key) DO UPDATE SET value=excluded.value, ts=excluded.ts",
            params![user, key, value, now()],
        )?;
        Ok(())
    }

    pub fn latest_summary(&self, user: &str) -> Result<String> {
        Ok(self
            .conn
            .query_row("SELECT text FROM summaries WHERE user_id=? ORDER BY id DESC LIMIT 1", params![user], |r| r.get(0))
            .optional()?
            .unwrap_or_default())
    }

    pub fn unsummarized_count(&self, user: &str) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM messages WHERE user_id=? AND summarized=0", params![user], |r| r.get(0))?)
    }

    /// Oldest unsummarized messages, leaving the `keep_recent` newest untouched.
    pub fn summarizable(&self, user: &str, keep_recent: i64) -> Result<Vec<Msg>> {
        let mut st = self.conn.prepare(
            "SELECT id,role,text FROM messages WHERE user_id=? AND summarized=0 ORDER BY id ASC LIMIT max(0,(SELECT COUNT(*) FROM messages WHERE user_id=? AND summarized=0) - ?)",
        )?;
        let v = st
            .query_map(params![user, user, keep_recent], |r| Ok(Msg { id: r.get(0)?, role: r.get(1)?, text: r.get(2)? }))?
            .collect::<Result<_, _>>()?;
        Ok(v)
    }

    pub fn save_summary(&self, user: &str, text: &str, upto: i64) -> Result<()> {
        self.conn.execute("INSERT INTO summaries(user_id,text,upto_message_id,ts) VALUES(?,?,?,?)", params![user, text, upto, now()])?;
        self.conn.execute("UPDATE messages SET summarized=1 WHERE user_id=? AND id<=?", params![user, upto])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::hash_embed;

    #[test]
    fn store_search_and_facts() {
        let s = Store::open(":memory:").unwrap();
        let id = s.add_message("u", "user", "my cat Tofu sleeps on the keyboard").unwrap();
        s.add_memory(
            "u",
            Some(id),
            "message",
            "my cat Tofu sleeps on the keyboard",
            "hash",
            &hash_embed("my cat Tofu sleeps on the keyboard"),
        )
        .unwrap();
        let id2 = s.add_message("u", "user", "berlin was rainy").unwrap();
        s.add_memory("u", Some(id2), "message", "berlin was rainy", "hash", &hash_embed("berlin was rainy")).unwrap();
        let hits = s.search("u", "hash", &hash_embed("tell me about my cat tofu"), 3, 0.2, &[]).unwrap();
        assert_eq!(hits.first().map(|m| m.text.as_str()), Some("my cat Tofu sleeps on the keyboard"));
        assert!(s.search("u", "hash", &hash_embed("tofu cat"), 3, 0.2, &[id]).unwrap().is_empty());
        s.upsert_fact("u", "likes", "ramen").unwrap();
        s.upsert_fact("u", "likes", "tea").unwrap();
        s.upsert_fact("u", "likes", "ramen").unwrap();
        s.upsert_fact("u", "name", "Ken").unwrap();
        s.upsert_fact("u", "name", "Kenji").unwrap();
        let f = s.facts("u").unwrap();
        assert!(f.contains(&("likes".into(), "ramen, tea".into())));
        assert!(f.contains(&("name".into(), "Kenji".into())));
    }

    #[test]
    fn summarization_window() {
        let s = Store::open(":memory:").unwrap();
        for i in 0..12 {
            s.add_message("u", "user", &format!("m{i}")).unwrap();
        }
        let old = s.summarizable("u", 4).unwrap();
        assert_eq!(old.len(), 8);
        s.save_summary("u", "sum", old.last().unwrap().id).unwrap();
        assert_eq!(s.unsummarized_count("u").unwrap(), 4);
        assert_eq!(s.latest_summary("u").unwrap(), "sum");
        assert_eq!(s.recent("u", 10).unwrap().len(), 4);
    }
}
