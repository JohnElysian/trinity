//! Run a reviewed SQL file against RMR_DATABASE (also useful for market audits).
use tokio_postgres::{NoTls, SimpleQueryMessage};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::var("RMR_DATABASE")?;
    let path = std::env::args().nth(1).ok_or("usage: db_sql <sql-file>")?;
    let sql = std::fs::read_to_string(path)?;
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await?;
    tokio::spawn(async move { if let Err(error) = connection.await { eprintln!("{error}"); } });
    for result in client.simple_query(&sql).await? {
        if let SimpleQueryMessage::Row(row) = result {
            println!("{}", (0..row.len()).map(|i| row.get(i).unwrap_or("NULL")).collect::<Vec<_>>().join("\t"));
        }
    }
    Ok(())
}
