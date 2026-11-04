use super::*;
use crate::{session::Session, test_support::Database};

async fn pilot(db: &Database) -> i64 {
    let id=db.id().await;
    db.db.execute(r#"INSERT INTO "character_" ("characterID","characterName","corporationID",balance)
        VALUES ($1,$2,1000168,1000000000)"#,&[&id,&format!("Market test {id}")]).await.unwrap();
    id as i64
}

fn order(station:i64,type_id:i64,price:f64,quantity:i64,bid:bool,duration:i64,item_id:Option<i64>) -> NewOrder {
    NewOrder {station,type_id,price,quantity,bid,range:-1,item_id,min_volume:1,duration,corp:false,existing:None,account:1000}
}

async fn run(db:&Database, me:i64, order:NewOrder)->Result<Effects,Refusal> {
    let mut client=crate::db::connect(&db.url).await.unwrap();
    let mut txn=ItemTxn::begin(&mut client).await.unwrap(); let mut fx=Effects::default();
    place(&mut txn,&mut fx,me,order,(None,0)).await?;
    fx.item_events=txn.commit().await?;
    Ok(fx)
}

async fn stock(db:&Database,me:i64,station:i64,type_id:i64,quantity:i64)->i64 {
    let mut client=crate::db::connect(&db.url).await.unwrap();
    let mut txn=ItemTxn::begin(&mut client).await.unwrap();
    let id=txn.create(NewItem{type_id,owner_id:me,location_id:station,flag:4,quantity,singleton:false}).await.unwrap();
    txn.commit().await.unwrap(); id
}

async fn balance(db:&Database,id:i64)->f64 {
    db.db.query_one(r#"SELECT balance FROM "character_" WHERE "characterID"=$1"#,&[&(id as i32)]).await.unwrap().get(0)
}

#[tokio::test]
#[ignore="requires isolated TRINITY_TEST_DATABASE"]
async fn market_starter_coverage_npc_buy_sell_and_wire_contracts() {
    let db=Database::load().await;
    let missing:i64=db.db.query_one(r#"WITH school AS (
        SELECT min(st."stationID") station FROM "chrSchools" s
        JOIN "crpNPCCorporations" n ON n."corporationID"=s."corporationID"
        JOIN "staStations" st ON st."corporationID"=s."corporationID" AND st."solarSystemID"=n."solarSystemID"
        GROUP BY s."schoolID")
        SELECT count(*) FROM school CROSS JOIN "invTypes" t CROSS JOIN (VALUES(0),(1)) side(bid)
        WHERE t."marketGroupID" IS NOT NULL AND NOT EXISTS(SELECT 1 FROM market_orders o
          WHERE o."stationID"=school.station AND o."typeID"=t."typeID" AND o.bid=side.bid
          AND o."starterSeed" AND o."orderState"=0 AND o."volRemaining">0)"#,&[]).await.unwrap().get(0);
    assert_eq!(missing,0);
    let me=pilot(&db).await;
    let price:f64=db.db.query_one(r#"SELECT min(price) FROM market_orders WHERE "stationID"=60014689 AND "typeID"=34 AND bid=0 AND "orderState"=0"#,&[]).await.unwrap().get(0);
    let before:i64=db.db.query_one(r#"SELECT sum("volRemaining")::bigint FROM market_orders WHERE "stationID"=60014689 AND "typeID"=34 AND bid=0"#,&[]).await.unwrap().get(0);
    run(&db,me,order(60014689,34,price,5,true,0,None)).await.unwrap();
    let item:i32=db.db.query_one(r#"SELECT "itemID" FROM entity WHERE "ownerID"=$1 AND "locationID"=60014689 AND "typeID"=34"#,&[&(me as i32)]).await.unwrap().get(0);
    assert!((balance(&db,me).await-(1e9-price*5.0)).abs()<0.001);
    let after:i64=db.db.query_one(r#"SELECT sum("volRemaining")::bigint FROM market_orders WHERE "stationID"=60014689 AND "typeID"=34 AND bid=0"#,&[]).await.unwrap().get(0);
    assert_eq!(before,after,"starter NPC supply is permanent");
    run(&db,me,order(60014689,34,0.01,3,false,0,Some(item as i64))).await.unwrap();
    let quantity:i32=db.db.query_one(r#"SELECT quantity FROM entity WHERE "itemID"=$1"#,&[&item]).await.unwrap().get(0);
    assert_eq!(quantity,2);
    let session=Session{char_id:Some(me),station_id:Some(60014689),solar_system_id2:Some(30004971),region_id:Some(10000064),..Default::default()};
    let ctx=db.ctx(&session);
    for method in ["StartupCheck","GetMarketGroups","GetOrders","GetStationAsks","GetSystemAsks","GetRegionBest","GetCharOrders","GetCorporationOrders","GetOldPriceHistory","GetNewPriceHistory"] {
        let args=if matches!(method,"GetOrders"|"GetOldPriceHistory"|"GetNewPriceHistory") {vec![Value::Int(34)]} else {vec![]};
        let call=Call{service:"marketProxy".into(),method:method.into(),args,kwargs:vec![]};
        let Some(Outcome::Value(value))=crate::svc::market::market_proxy(&call,&ctx).await else {panic!("{method} failed")};
        let table=eve_marshal::StringTable::default();
        let bytes=eve_marshal::save(&value,&table).unwrap();
        assert_eq!(eve_marshal::load(&bytes,&table).unwrap(),value,"{method} marshal roundtrip");
        assert!(eve_proto::maybe_compress(&bytes,0).len()<1_000_000,"{method} fits client frame");
        if method.ends_with("PriceHistory") {assert!(matches!(value,Value::Reduce{..}));}
        if method=="GetOrders" {
            let Value::Instance{state,..}=value else {panic!("versioned order result")};
            let bytes=state.seq().unwrap()[1].as_bytes().unwrap();
            let orders=eve_marshal::load(bytes,&table).unwrap();
            for side in orders.seq().unwrap() {
                let Value::Reduce{args,..}=side else {panic!("orders must be native RowLists")};
                let packed=args.seq().unwrap()[0].seq().unwrap();
                let descriptor=eve_marshal::dbrow::DbRowDescriptor::from_value(&packed[0]).unwrap();
                assert!(descriptor.index_of("accountKey").is_some());
                assert!(packed[1].seq().unwrap().iter().all(|row|matches!(row,Value::DbRow(_))));
            }
        }
    }
    let call=Call{service:"marketProxy".into(),method:"CharGetNewTransactions".into(),args:vec![Value::Int(1),Value::Int(34)],kwargs:vec![]};
    let Some(Outcome::Value(Value::Instance{state,..}))=crate::svc::wallet::market_proxy(&call,&ctx).await else {panic!("transactions failed")};
    let Value::Dict(fields)=*state else {panic!()};
    let lines=&fields.iter().find(|(k,_)|*k==Value::bytes("lines")).unwrap().1;
    assert_eq!(lines.seq().unwrap().len(),1,"purchase filter");
    assert_eq!(lines.seq().unwrap()[0].seq().unwrap()[10],Value::Int(0));
}

#[tokio::test]
#[ignore="requires isolated TRINITY_TEST_DATABASE"]
async fn market_player_matching_repricing_refunds_expiry_and_atomic_refusal() {
    let db=Database::load().await; let seller=pilot(&db).await; let buyer=pilot(&db).await;
    let volume_before:i64=db.db.query_one(r#"SELECT coalesce(sum(volume),0)::bigint FROM market_history WHERE "typeID"=35"#,&[]).await.unwrap().get(0);
    let item=stock(&db,seller,60000004,35,10).await;
    let ask=run(&db,seller,order(60000004,35,3.0,10,false,1,Some(item))).await.ok().unwrap().orders[0].0;
    let bid=run(&db,buyer,order(60000004,35,2.0,5,true,1,None)).await.ok().unwrap().orders[0].0;
    assert_eq!(balance(&db,buyer).await,1e9-110.0);
    let mut client=crate::db::connect(&db.url).await.unwrap();
    let mut txn=ItemTxn::begin(&mut client).await.unwrap();
    assert!(modify(&mut txn,&mut Effects::default(),seller,ask,2.0,(None,0)).await.is_ok());
    txn.commit().await.unwrap();
    let r=db.db.query_one(r#"SELECT "volRemaining",escrow,"orderState" FROM market_orders WHERE "orderID"=$1"#,&[&(bid as i32)]).await.unwrap();
    assert_eq!((r.get::<_,i32>(0),r.get::<_,f64>(1),r.get::<_,i32>(2)),(0,0.0,2));
    let r=db.db.query_one(r#"SELECT "volRemaining" FROM market_orders WHERE "orderID"=$1"#,&[&(ask as i32)]).await.unwrap(); assert_eq!(r.get::<_,i32>(0),5);
    let mut txn=ItemTxn::begin(&mut client).await.unwrap();
    assert!(cancel(&mut txn,&mut Effects::default(),buyer,ask,(None,0)).await.is_err()); drop(txn);
    let mut txn=ItemTxn::begin(&mut client).await.unwrap();
    assert!(cancel(&mut txn,&mut Effects::default(),seller,ask,(None,0)).await.is_ok()); txn.commit().await.unwrap();
    let returned:i64=db.db.query_one(r#"SELECT sum(quantity)::bigint FROM entity WHERE "ownerID"=$1 AND "typeID"=35"#,&[&(seller as i32)]).await.unwrap().get(0); assert_eq!(returned,5);
    let next=run(&db,buyer,order(60000004,36,2.0,7,true,1,None)).await.ok().unwrap().orders[0].0;
    db.db.execute(r#"UPDATE market_orders SET issued=1 WHERE "orderID"=$1"#,&[&(next as i32)]).await.unwrap();
    let before=balance(&db,buyer).await;
    for _ in 0..2 {let mut txn=ItemTxn::begin(&mut client).await.unwrap(); assert!(expire_orders(&mut txn,&mut Effects::default()).await.is_ok()); txn.commit().await.unwrap();}
    assert_eq!(balance(&db,buyer).await,before+14.0,"expiry refunds once");
    let before=balance(&db,buyer).await;
    assert!(run(&db,buyer,order(60000004,36,1.0,100,true,0,None)).await.is_err());
    assert_eq!(balance(&db,buyer).await,before,"failed immediate order rolls back all effects");
    let volume:i64=db.db.query_one(r#"SELECT sum(volume)::bigint FROM market_history WHERE "typeID"=35"#,&[]).await.unwrap().get(0); assert_eq!(volume-volume_before,5,"one history trade, not two wallet sides");
}

#[tokio::test]
#[ignore="requires isolated TRINITY_TEST_DATABASE"]
async fn market_competing_buyers_cannot_duplicate_stock() {
    let db=Database::load().await; let seller=pilot(&db).await; let a=pilot(&db).await; let b=pilot(&db).await;
    let item=stock(&db,seller,60000004,37,1).await;
    assert!(run(&db,seller,order(60000004,37,3.0,1,false,1,Some(item))).await.is_ok());
    let (a_result,b_result)=tokio::join!(run(&db,a,order(60000004,37,3.0,1,true,0,None)),run(&db,b,order(60000004,37,3.0,1,true,0,None)));
    assert_eq!(usize::from(a_result.is_ok())+usize::from(b_result.is_ok()),1);
    let n:i64=db.db.query_one(r#"SELECT sum(quantity)::bigint FROM entity WHERE "ownerID"=ANY($1) AND "typeID"=37"#,&[&vec![a as i32,b as i32]]).await.unwrap().get(0); assert_eq!(n,1);
}

#[tokio::test]
#[ignore="requires isolated TRINITY_TEST_DATABASE"]
async fn market_margin_corporation_delivery_records_notifications_and_permissions() {
    let db=Database::load().await; let issuer=pilot(&db).await; let seller=pilot(&db).await;
    let corp=db.id().await as i64;
    db.db.execute(r#"INSERT INTO corporation ("corporationID","corporationName",balance) VALUES ($1,$2,1000000)"#,
        &[&(corp as i32),&format!("Market corporation {corp}")]).await.unwrap();
    let skill=stock(&db,issuer,issuer,16597,1).await;
    db.db.execute(r#"UPDATE entity SET flag=7,singleton=1 WHERE "itemID"=$1"#,&[&(skill as i32)]).await.unwrap();
    db.db.execute(r#"INSERT INTO entity_attributes ("itemID","attributeID","valueInt") VALUES ($1,280,5)"#,&[&(skill as i32)]).await.unwrap();
    let mut client=crate::db::connect(&db.url).await.unwrap();
    let mut txn=ItemTxn::begin(&mut client).await.unwrap(); let mut fx=Effects::default();
    let mut bid=order(60000004,38,100.0,10,true,1,None); bid.corp=true; bid.range=32767;
    place(&mut txn,&mut fx,issuer,bid,(Some(corp),1)).await.unwrap();
    let order_id=fx.orders[0].0; txn.commit().await.unwrap();
    let escrow:f64=db.db.query_one(r#"SELECT escrow FROM market_orders WHERE "orderID"=$1"#,&[&(order_id as i32)]).await.unwrap().get(0);
    assert_eq!(escrow,237.31);
    let other_station:i32=db.db.query_one(r#"SELECT "stationID" FROM "staStations" WHERE "regionID"=(SELECT "regionID" FROM "staStations" WHERE "stationID"=60000004) AND "stationID"<>60000004 ORDER BY "stationID" LIMIT 1"#,&[]).await.unwrap().get(0);
    let item=stock(&db,seller,other_station as i64,38,10).await;
    let (mut issuer_rx,mut seller_rx)={let mut w=db.world.lock();let a=w.connect(1);let b=w.connect(2);
        for (client,char_id,corp_id) in [(1,issuer,corp),(2,seller,1000168)] {
            w.set_presence(client,Some(crate::world::Presence{char_id,corp_id,solar_system_id:30000142,docked:true,station_id:Some(other_station as i64),role:3}));
        } (a,b)};
    let fx=run(&db,seller,order(other_station as i64,38,100.0,10,false,0,Some(item))).await.unwrap();
    let session=Session{char_id:Some(seller),..Default::default()}; tell(&db.ctx(&session),fx).await;
    let destination:i32=db.db.query_one(r#"SELECT "locationID" FROM entity WHERE "ownerID"=$1 AND "typeID"=38 AND flag=62"#,&[&(corp as i32)]).await.unwrap().get(0);
    assert_eq!(destination,other_station,"a ranged buy does not teleport purchased goods");
    let remaining:f64=db.db.query_one(r#"SELECT balance FROM corporation WHERE "corporationID"=$1"#,&[&(corp as i32)]).await.unwrap().get(0);
    assert!((remaining-998900.0).abs()<0.001,"escrow and later funding together pay exactly the trade plus fee");
    let r=db.db.query_one(r#"SELECT "characterID","corporationID","isCorp",quantity,"stationID" FROM market_transactions WHERE "corporationID"=$1"#,&[&(corp as i32)]).await.unwrap();
    assert_eq!(r.get::<_,i32>(0),issuer as i32); assert_eq!(r.get::<_,i16>(2),1); assert_eq!(r.get::<_,i32>(4),other_station);
    for rx in [&mut issuer_rx,&mut seller_rx] {
        let mut item_event=false;let mut account_event=false;let mut order_event=false;
        while let Ok(crate::world::Push::Notify(note))=rx.try_recv() {
            match note.event {"OnItemChange"=>item_event=true,"OnAccountChange"=>account_event=true,
                "OnOwnOrderChanged"=>{assert_eq!(note.args.seq().unwrap().len(),3);order_event=true;},_=>{}}
        }
        assert!(item_event && account_event && order_event,"both trade participants receive their changes");
    }
    let mut session=Session{char_id:Some(issuer),corp_id:Some(corp),roles_at_all:256,..Default::default()};
    let call=Call{service:"marketProxy".into(),method:"CorpGetNewTransactions".into(),args:vec![],kwargs:vec![]};
    assert!(matches!(crate::svc::wallet::market_proxy(&call,&db.ctx(&session)).await,Some(Outcome::Value(_))));
    session.roles_at_all=0;
    assert!(matches!(crate::svc::wallet::market_proxy(&call,&db.ctx(&session)).await,Some(Outcome::UserError{..})));
}

#[tokio::test]
#[ignore="requires isolated TRINITY_TEST_DATABASE"]
async fn market_rejects_forged_assets_insufficient_funds_bad_ranges_and_order_limits() {
    let db=Database::load().await; let me=pilot(&db).await;let stranger=pilot(&db).await;
    let foreign=stock(&db,stranger,60000004,39,1).await;
    assert!(run(&db,me,order(60000004,39,1.0,1,false,1,Some(foreign))).await.is_err());
    for price in [f64::NAN,f64::INFINITY,-1.0,0.0] {assert!(run(&db,me,order(60000004,39,price,1,true,1,None)).await.is_err());}
    for _ in 0..5 {run(&db,me,order(60000004,39,0.01,1,true,1,None)).await.unwrap();}
    let before=balance(&db,me).await;
    assert!(run(&db,me,order(60000004,39,0.01,1,true,1,None)).await.is_err());
    assert_eq!(balance(&db,me).await,before);
    db.db.execute(r#"UPDATE "character_" SET balance=0 WHERE "characterID"=$1"#,&[&(stranger as i32)]).await.unwrap();
    assert!(run(&db,stranger,order(60014689,34,1e6,1,true,0,None)).await.is_err());
    let session=Session{char_id:Some(me),station_id:Some(60014689),solar_system_id2:Some(30004971),region_id:Some(10000064),..Default::default()};
    assert!(access(&db.ctx(&session),60014659,16594,true,None).await.is_err(),"different region");
    let other:i32=db.db.query_one(r#"SELECT "stationID" FROM "staStations" WHERE "regionID"=10000064 AND "stationID"<>60014689 LIMIT 1"#,&[]).await.unwrap().get(0);
    assert!(access(&db.ctx(&session),other as i64,16594,false,None).await.is_err(),"untrained remote order");
    assert!(access(&db.ctx(&session),other as i64,16594,true,None).await.is_ok(),"immediate remote purchase");
}

#[tokio::test]
#[ignore="requires isolated TRINITY_TEST_DATABASE"]
async fn market_division_wallet_selection_funding_escrow_refund_and_journal_stay_separate() {
    let db=Database::load().await;let me=pilot(&db).await;let corp=db.id().await as i64;
    db.db.execute(r#"INSERT INTO corporation ("corporationID","corporationName",balance) VALUES ($1,$2,1000000)"#,
        &[&(corp as i32),&format!("Division market {corp}")]).await.unwrap();
    let mut session=Session{char_id:Some(me),corp_id:Some(corp),roles_at_all:1,..Default::default()};
    let transfer=Call{service:"account".into(),method:"GiveCashFromCorpAccount".into(),
        args:vec![Value::Int(corp),Value::Double(10000.0),Value::Int(1000)],
        kwargs:vec![(Value::bytes("toAccountKey"),Value::Int(1002))]};
    assert!(matches!(crate::svc::wallet::account(&transfer,&db.ctx(&session)).await,Some(Outcome::Value(_))));
    crate::svc::wallet::select_account(&db.db,&mut session,1002).await.unwrap();
    assert_eq!(session.corp_account_key,Some(1002));
    let saved:i32=db.db.query_one(r#"SELECT "corpAccountKey" FROM "character_" WHERE "characterID"=$1"#,&[&(me as i32)]).await.unwrap().get(0);assert_eq!(saved,1002);
    let mut client=crate::db::connect(&db.url).await.unwrap();
    let mut txn=ItemTxn::begin(&mut client).await.unwrap();let mut fx=Effects::default();
    let mut bid=order(60000004,40,100.0,10,true,1,None);bid.corp=true;bid.account=1002;
    place(&mut txn,&mut fx,me,bid,(Some(corp),1)).await.unwrap();let id=fx.orders[0].0;txn.commit().await.unwrap();
    let amount:f64=db.db.query_one(r#"SELECT balance FROM corporation_wallets WHERE "corporationID"=$1 AND "accountKey"=1002"#,&[&(corp as i32)]).await.unwrap().get(0);assert_eq!(amount,8900.0);
    let mut txn=ItemTxn::begin(&mut client).await.unwrap();cancel(&mut txn,&mut Effects::default(),me,id,(Some(corp),1)).await.unwrap();txn.commit().await.unwrap();
    let amount:f64=db.db.query_one(r#"SELECT balance FROM corporation_wallets WHERE "corporationID"=$1 AND "accountKey"=1002"#,&[&(corp as i32)]).await.unwrap().get(0);assert_eq!(amount,9900.0);
    let amount:f64=db.db.query_one(r#"SELECT balance FROM corporation WHERE "corporationID"=$1"#,&[&(corp as i32)]).await.unwrap().get(0);assert_eq!(amount,990000.0);
    let n:i64=db.db.query_one(r#"SELECT count(*) FROM market_journal WHERE "characterID"=$1 AND "accountKey"=1002"#,&[&(corp as i32)]).await.unwrap().get(0);assert_eq!(n,4);
    session.roles_at_all=0;
    assert!(crate::svc::wallet::select_account(&db.db,&mut session,1003).await.is_err());
    assert!(matches!(crate::svc::wallet::account(&transfer,&db.ctx(&session)).await,Some(Outcome::UserError{..})));
}
