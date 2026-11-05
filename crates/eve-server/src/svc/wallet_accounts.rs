//! Corporation wallet divisions used by market orders and wallet transfers.
use super::{Entry, Transfer, post};
use crate::store::{Store, StoreError, Tx, Value};

pub fn can_take(roles:i64,key:i32)->bool {
    (1000..=1006).contains(&key) && (roles & 1 != 0
        || roles & (134_217_728i64 << (key-1000)) != 0
        || (key==1000 && roles & (256 | 18_014_398_509_481_984)!=0))
}

pub fn post_account(tx:&mut Tx<'_>,owner:i64,key:i32,delta:f64,e:&Entry<'_>)->Result<f64,Transfer> {
    if key==1000 {return post(tx,owner,delta,e);}
    if !(1001..=1006).contains(&key) || !delta.is_finite() {return Err(Transfer::NoSuchOwner);}
    if tx.table("corporation").get_id(owner).is_none() {
        return Err(Transfer::NoSuchOwner);
    }
    let wallet=[Value::Int(owner),Value::Int(key.into())];
    tx.insert_or_ignore("corporation_wallets",&[("corporationID",wallet[0].clone()),("accountKey",wallet[1].clone()),("balance",Value::Float(0.0))])
        .map_err(Transfer::Db)?;
    let row=tx.table("corporation_wallets").get(&wallet).expect("the wallet division was just made");
    let (row,have)=(row.id,row.float("balance").unwrap_or(0.0));
    if have+delta<0.0 {return Err(Transfer::NotEnough{need:-delta,have});}
    let balance=have+delta;
    tx.update("corporation_wallets",row,&[("balance",Value::Float(balance))]).map_err(Transfer::Db)?;
    tx.insert("market_journal",&[
        ("characterID",Value::Int(owner)),("transDate",Value::Int(crate::bulkdata::filetime_now())),("refTypeID",Value::Int(e.ref_type.into())),
        ("ownerID1",Value::Int(e.payer)),("ownerID2",Value::Int(e.payee)),("argID1",Value::Text(e.arg_id1.into())),("accountID",Value::Int(0)),
        ("amount",Value::Float(delta)),("balance",Value::Float(balance)),("reason",Value::Text(e.reason.into())),("accountKey",Value::Int(key.into())),
    ]).map_err(Transfer::Db)?;
    Ok(balance)
}

pub async fn select_account(store:&Store,session:&mut crate::session::Session,key:i32)->Result<(),String> {
    let me=session.char_id.ok_or("Select a character first.")?;
    if session.corp_id.is_none() || !can_take(session.corp_role(),key) {return Err("You do not have access to that corporation wallet division.".into());}
    store.write(|tx| {
        let Some(row)=tx.table("character_").get_id(me).map(|r|r.id) else {return Ok(())};
        tx.update("character_",row,&[("corpAccountKey",Value::Int(key.into()))])
    }).map_err(|_:StoreError|"The selected wallet division could not be saved.")?;
    session.corp_account_key=Some(key);
    Ok(())
}

/// A refused transfer, which undoes the store write it ends.
struct Refused(Transfer);

impl From<StoreError> for Refused {
    fn from(e:StoreError)->Refused {Refused(Transfer::Db(e))}
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
    let result:Result<Vec<(i64,i32,f64)>,Refused>=ctx.store.write(|tx| {
        let entry=Entry{ref_type:if corp {37} else {10},payer,payee,arg_id1:"",reason:&reason};
        let mut sides=vec![(payer,from,-amount),(payee,to,amount)]; sides.sort_by_key(|(id,key,_)|(*id,*key));
        let mut changes=Vec::new();
        for (owner,key,delta) in sides {changes.push((owner,key,post_account(tx,owner,key,delta,&entry).map_err(Refused)?));}
        Ok(changes)
    });
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
        Err(Refused(Transfer::NotEnough{need,have}))=>Outcome::not_enough_money(need,have),
        Err(Refused(Transfer::NoSuchOwner))=>Outcome::custom("That wallet does not exist."),
        Err(Refused(Transfer::Db(error)))=>{tracing::error!("wallet division transfer: {}",crate::logging::chain(&error));Outcome::custom("The wallet transfer could not be completed.")}
    }
}
