use rusqlite::{Connection, Result};

pub fn init_db() -> Result<Connection> {
    let conn = Connection::open("service.db")?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS pages (
            id INTEGER PRIMARY KEY,
            url TEXT UNIQUE,
            title TEXT,
            content TEXT
        )",
        [],
    )?;

    Ok(conn)
}
