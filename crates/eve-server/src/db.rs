use tokio_postgres::{Client, NoTls};
use tracing::{error, info};

mod recovery;
pub use recovery::{AcquireError, Reconnect, Shared};

#[derive(Debug, Clone)]
pub struct Account {
    pub account_id: i64,
    pub name: String,
    pub role: i64,
    pub language_id: Option<String>,
}

pub async fn connect(url: &str) -> Result<Client, tokio_postgres::Error> {
    let mut config: tokio_postgres::Config = url.parse()?;
    if config.get_connect_timeout().is_none() {
        config.connect_timeout(std::time::Duration::from_secs(5));
    }
    let (client, connection) = config.connect(NoTls).await?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            error!("database connection lost: {err}", err = crate::logging::chain(&e));
        }
    });
    Ok(client)
}

pub async fn authenticate(
    db: &Client,
    username: &str,
    password: &str,
) -> Result<Option<Account>, tokio_postgres::Error> {
    let rows = db
        .query(
            r#"SELECT "accountID", "accountName", "password", "role", "banned", "languageID"
                 FROM "account" WHERE "accountName" = $1"#,
            &[&username],
        )
        .await?;
    let Some(row) = rows.first() else {
        return if crate::config::auto_create_accounts() && !username.trim().is_empty() && !password.is_empty() {
            create_account(db, username, password).await
        } else {
            Ok(None)
        };
    };
    if row.get::<_, i16>(4) != 0 {
        info!("{username:?} is banned");
        return Ok(None);
    }

    let stored: String = row.get(2);
    if stored != password {
        return Ok(None);
    }
    let account_id: i32 = row.get(0);
    Ok(Some(Account {
        account_id: account_id as i64,
        name: row.get(1),
        // bigint: roles above 2^31 exist (ROLE_TRANSLATIONEDITOR = 2^32).
        role: row.get(3),
        language_id: row.get(5),
    }))
}

/// A player account made at its first login (`autoCreateAccounts`).
async fn create_account(db: &Client, username: &str, password: &str) -> Result<Option<Account>, tokio_postgres::Error> {
    let row = db
        .query_opt(
            r#"INSERT INTO "account" ("accountName", "password", "role", "createdAt") VALUES ($1, $2, 0, $3)
               ON CONFLICT ("accountName") DO NOTHING RETURNING "accountID""#,
            &[&username, &password, &crate::bulkdata::filetime_now()],
        )
        .await?;
    if let Some(row) = row {
        info!("account {username:?} made at its first login");
        return Ok(Some(Account { account_id: row.get::<_, i32>(0) as i64, name: username.to_string(), role: 0, language_id: None }));
    }
    // Another first login won the insert. Verify its password instead of
    // treating a name collision as successful authentication.
    Ok(db.query_opt(r#"SELECT "accountID",role,"languageID" FROM account
        WHERE "accountName"=$1 AND password=$2 AND banned=0"#,&[&username,&password]).await?
        .map(|r|Account{account_id:r.get::<_,i32>(0) as i64,name:username.into(),role:r.get(1),language_id:r.get(2)}))
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE pointing at a disposable imported database"]
    async fn concurrent_first_login_never_bypasses_password_checks() {
        let f=crate::test_support::Database::load().await;
        let name=format!("concurrent-login-{}",f.id().await);
        let (a,b)=tokio::join!(super::create_account(&f.db,&name,"one"),super::create_account(&f.db,&name,"two"));
        let a=a.unwrap(); let b=b.unwrap();
        assert_ne!(a.is_some(),b.is_some());
        let password=if a.is_some(){"one"}else{"two"};
        let (a,b)=tokio::join!(super::create_account(&f.db,&name,password),super::create_account(&f.db,&name,password));
        assert_eq!(a.unwrap().unwrap().account_id,b.unwrap().unwrap().account_id);
        f.db.execute(r#"DELETE FROM account WHERE "accountName"=$1"#,&[&name]).await.unwrap();
    }
}

/// Makes the config file's accounts, or brings them into line with it:
/// password, GM roles and ban. Returns their accountIDs, in the file's order.
pub async fn sync_accounts(db: &Client, accounts: &[crate::config::Account]) -> Result<Vec<i64>, tokio_postgres::Error> {
    let now = crate::bulkdata::filetime_now();
    // Accounts once made with IDs of their own leave the sequence behind.
    let sql = r#"SELECT setval('"account_accountID_seq"', GREATEST((SELECT max("accountID") FROM "account"), 1))"#;
    db.execute(sql, &[]).await?;
    let mut ids = Vec::with_capacity(accounts.len());
    for a in accounts {
        let banned = i16::from(a.banned);
        let row = db
            .query_one(
                r#"INSERT INTO "account" ("accountName", "password", "role", "banned", "createdAt") VALUES ($1, $2, $3, $4, $5)
                   ON CONFLICT ("accountName") DO UPDATE SET "password" = $2, "passwordAlgo" = 'plain', "role" = $3, "banned" = $4
                   RETURNING "accountID""#,
                &[&a.username, &a.password, &a.role(), &banned, &now],
            )
            .await?;
        ids.push(row.get::<_, i32>(0) as i64);
    }
    Ok(ids)
}
