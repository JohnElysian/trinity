//! Corporation wallet divisions used by market orders and wallet transfers.
use super::{Entry, Transfer, post};
use tokio_postgres::Transaction;

pub fn can_take(roles:i64,key:i32)->bool {
    (1000..=1006).contains(&key) && (roles & 1 != 0
        || roles & (134_217_728i64 << (key-1000)) != 0
        || (key==1000 && roles & (256 | 18_014_398_509_481_984)!=0))
}

pub async fn post_account(tx:&Transaction<'_>,owner:i64,key:i32,delta:f64,e:&Entry<'_>)->Result<f64,Transfer> {
    if key==1000 {return post(tx,owner,delta,e).await;}
    if !(1001..=1006).contains(&key) || !delta.is_finite() {return Err(Transfer::NoSuchOwner);}
    let id=owner as i32;
    if tx.query_opt(r#"SELECT 1 FROM corporation WHERE "corporationID"=$1"#,&[&id]).await.map_err(Transfer::Db)?.is_none() {
        return Err(Transfer::NoSuchOwner);
    }
    tx.execute(r#"INSERT INTO corporation_wallets ("corporationID","accountKey",balance) VALUES ($1,$2,0) ON CONFLICT DO NOTHING"#,
        &[&id,&key]).await.map_err(Transfer::Db)?;
    let have:f64=tx.query_one(r#"SELECT balance FROM corporation_wallets WHERE "corporationID"=$1 AND "accountKey"=$2 FOR UPDATE"#,
        &[&id,&key]).await.map_err(Transfer::Db)?.get(0);
    if have+delta<0.0 {return Err(Transfer::NotEnough{need:-delta,have});}
    let balance=have+delta;
    tx.execute(r#"UPDATE corporation_wallets SET balance=$3 WHERE "corporationID"=$1 AND "accountKey"=$2"#,
        &[&id,&key,&balance]).await.map_err(Transfer::Db)?;
    tx.execute(r#"INSERT INTO market_journal
        ("characterID","transDate","refTypeID","ownerID1","ownerID2","argID1","accountID",amount,balance,reason,"accountKey")
        VALUES ($1,$2,$3,$4,$5,$6,0,$7,$8,$9,$10)"#,
        &[&id,&crate::bulkdata::filetime_now(),&e.ref_type,&(e.payer as i32),&(e.payee as i32),&e.arg_id1,&delta,&balance,&e.reason,&key]).await.map_err(Transfer::Db)?;
    Ok(balance)
}

pub async fn select_account(db:&tokio_postgres::Client,session:&mut crate::session::Session,key:i32)->Result<(),String> {
    let me=session.char_id.ok_or("Select a character first.")?;
    if session.corp_id.is_none() || !can_take(session.corp_role(),key) {return Err("You do not have access to that corporation wallet division.".into());}
    db.execute(r#"UPDATE "character_" SET "corpAccountKey"=$2 WHERE "characterID"=$1"#,&[&(me as i32),&key])
        .await.map_err(|_|"The selected wallet division could not be saved.")?;
    session.corp_account_key=Some(key);
    Ok(())
}

pub async fn transfer_accounts(ctx:&crate::services::Ctx<'_>,call:&crate::services::Call,corp:bool)->crate::services::Outcome {
    use crate::services::Outcome;
    use eve_marshal::Value;
    let Some(me)=ctx.session.char_id else {return Outcome::custom("Select a character first.")};
    let payer=if corp {ctx.session.corp_id.unwrap_or(0)} else {me};
    let from=if corp {call.arg_i64(2).unwrap_or(ctx.session.corp_account_key.unwrap_or(1000) as i64)} else {1000};
    let to=call.kwarg("toAccountKey").and_then(Value::as_i64).unwrap_or(1000);
    let payee=call.arg_id(0);
    let amount=call.args.get(1).and_then(Value::as_f64).unwrap_or(0.0);
    if !(1000..=1006).contains(&from) || !(1000..=1006).contains(&to) || !amount.is_finite() || amount<=0.0 {
        return Outcome::custom("Enter valid wallet divisions and an amount.");
    }
    let (from,to)=(from as i32,to as i32);
    if corp && !can_take(ctx.session.corp_role(),from) {return Outcome::custom("You may not withdraw from that wallet division.");}
    if payer==payee && from==to {return Outcome::Value(Value::None);}
    let reason=call.kwarg("reason").and_then(Value::as_string).or_else(||if !corp {call.arg_text(2)} else {None}).unwrap_or_default();
    let reason:String=reason.chars().take(40).collect();
    let result:Result<Vec<(i64,i32,f64)>,Transfer>=async {
        let mut client=crate::db::connect(ctx.db_url).await.map_err(Transfer::Db)?;
        let tx=client.transaction().await.map_err(Transfer::Db)?;
        tx.execute("SELECT pg_advisory_xact_lock(45017,2)",&[]).await.map_err(Transfer::Db)?;
        let entry=Entry{ref_type:if corp {37} else {10},payer,payee,arg_id1:"",reason:&reason};
        let mut sides=vec![(payer,from,-amount),(payee,to,amount)]; sides.sort_by_key(|(id,key,_)|(*id,*key));
        let mut changes=Vec::new();
        for (owner,key,delta) in sides {changes.push((owner,key,post_account(&tx,owner,key,delta,&entry).await?));}
        tx.commit().await.map_err(Transfer::Db)?;
        Ok(changes)
    }.await;
    match result {
        Ok(changes)=>{
            let world=ctx.world.lock();
            for (owner,key,balance) in changes {
                let mut clients=world.clients_of(owner);clients.extend(world.corp_members(owner));clients.sort_unstable();clients.dedup();
                let name=if key==1000 {"cash".into()} else {format!("cash{}",key-999)};
                world.notify(&clients,"OnAccountChange",Value::Tuple(vec![Value::bytes(name),Value::Int(owner),Value::Double(balance)]));
            }
            Outcome::Value(Value::None)
        },
        Err(Transfer::NotEnough{need,have})=>Outcome::not_enough_money(need,have),
        Err(Transfer::NoSuchOwner)=>Outcome::custom("That wallet does not exist."),
        Err(Transfer::Db(error))=>{tracing::error!("wallet division transfer: {}",crate::logging::chain(&error));Outcome::custom("The wallet transfer could not be completed.")}
    }
}
