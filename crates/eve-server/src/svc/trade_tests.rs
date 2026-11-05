use super::*;
use crate::{session::Session, test_support::Database};
use crate::store::Value as Cell;

async fn pilot(db: &Database) -> i64 {
    let id=i64::from(db.id().await);
    db.store.write(|tx|tx.insert("character_",&[("characterID",Cell::Int(id)),("characterName",Cell::Text(format!("Market test {id}").into())),
        ("corporationID",Cell::Int(1000168)),("balance",Cell::Float(1000000000.0))])).unwrap();
    id
}

fn order(station:i64,type_id:i64,price:f64,quantity:i64,bid:bool,duration:i64,item_id:Option<i64>) -> NewOrder {
    NewOrder {station,type_id,price,quantity,bid,range:-1,item_id,min_volume:1,duration,corp:false,existing:None,account:1000}
}

async fn run(db:&Database, me:i64, order:NewOrder)->Result<Effects,Refusal> {
    db.store.write(|tx| {
        let mut txn=ItemTxn::new(tx); let mut fx=Effects::default();
        place(&mut txn,&mut fx,me,order,(None,0))?;
        fx.item_events=txn.events();
        Ok(fx)
    })
}

async fn stock(db:&Database,me:i64,station:i64,type_id:i64,quantity:i64)->i64 {
    db.store.write(|tx|ItemTxn::new(tx).create(NewItem{type_id,owner_id:me,location_id:station,flag:4,quantity,singleton:false})).unwrap()
}

async fn balance(db:&Database,id:i64)->f64 {
    db.store.read(|d|d.table("character_").get_id(id).and_then(|c|c.float("balance"))).unwrap()
}

/// Sets columns of the row whose single-column key is `id`.
fn update(db:&Database,table:&str,id:i64,sets:&[(&str,Cell)]) {
    db.store.write(|tx|{let row=tx.table(table).get_id(id).unwrap().id;tx.update(table,row,sets)}).unwrap();
}

fn order_row<T>(db:&Database,id:i64,read:impl FnOnce(&crate::store::Row)->T)->T {
    db.store.read(|d|read(&d.table("market_orders").get_id(id).unwrap()))
}

/// The ask (bid 0) or bid (1) orders of a type at a station.
fn station_orders(db:&Database,station:i64,type_id:i64,bid:i64)->Vec<(Option<f64>,Option<i64>,Option<i64>)> {
    db.store.read(|d|d.table("market_orders").find(&["stationID","typeID"],&[Cell::Int(station),Cell::Int(type_id)])
        .filter(|o|o.int("bid")==Some(bid)).map(|o|(o.float("price"),o.int("orderState"),o.int("volRemaining"))).collect())
}

/// How much of a type the owners hold.
fn held(db:&Database,owners:&[i64],type_id:i64)->i64 {
    db.store.read(|d|owners.iter().flat_map(|o|d.table("entity").find(&["ownerID","typeID"],&[Cell::Int(*o),Cell::Int(type_id)]))
        .filter_map(|e|e.int("quantity")).sum())
}

fn traded_volume(db:&Database,type_id:i64)->i64 {
    db.store.read(|d|d.table("market_history").find(&["typeID"],&[Cell::Int(type_id)]).filter_map(|h|h.int("volume")).sum())
}

#[tokio::test]
async fn market_starter_coverage_npc_buy_sell_and_wire_contracts() {
    let db=Database::load().await;
    let tables=crate::sde::get().tables();
    let (schools,corps,systems)=crate::sde::market::schools(tables);
    let (types,_,_)=crate::sde::market::marketable(tables);
    let missing=db.store.read(|d| {
        // Each school's first station of its corporation in its system.
        let mut school_stations=std::collections::BTreeMap::<i32,i64>::new();
        for ((school,corp),system) in schools.iter().zip(&corps).zip(&systems) {
            for st in d.table("staStations").find(&["solarSystemID"],&[Cell::Int((*system).into())]).filter(|st|st.int("corporationID")==Some((*corp).into())) {
                let Some(station)=st.int("stationID") else {continue};
                let first=school_stations.entry(*school).or_insert(station);
                *first=(*first).min(station);
            }
        }
        let mut missing=0;
        for station in school_stations.values() {
            for type_id in &types {
                for bid in [0,1] {
                    let stocked=d.table("market_orders").find(&["stationID","typeID"],&[Cell::Int(*station),Cell::Int((*type_id).into())])
                        .any(|o|o.int("bid")==Some(bid) && o.bool("starterSeed")==Some(true) && o.int("orderState")==Some(0) && o.int("volRemaining").is_some_and(|v|v>0));
                    if !stocked {missing+=1;}
                }
            }
        }
        missing
    });
    assert_eq!(missing,0);
    let me=pilot(&db).await;
    let price=station_orders(&db,60014689,34,0).into_iter().filter(|o|o.1==Some(0)).filter_map(|o|o.0).min_by(f64::total_cmp).unwrap();
    let supply=|db:&Database|station_orders(db,60014689,34,0).into_iter().filter_map(|o|o.2).sum::<i64>();
    let before=supply(&db);
    run(&db,me,order(60014689,34,price,5,true,0,None)).await.unwrap();
    let item=db.store.read(|d|d.table("entity").find(&["ownerID","locationID"],&[Cell::Int(me),Cell::Int(60014689)])
        .filter(|e|e.int("typeID")==Some(34)).filter_map(|e|e.int("itemID")).next()).unwrap();
    assert!((balance(&db,me).await-(1e9-price*5.0)).abs()<0.001);
    let after=supply(&db);
    assert_eq!(before,after,"starter NPC supply is permanent");
    run(&db,me,order(60014689,34,0.01,3,false,0,Some(item))).await.unwrap();
    let quantity=db.store.read(|d|d.table("entity").get_id(item).and_then(|e|e.int("quantity"))).unwrap();
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
async fn market_player_matching_repricing_refunds_expiry_and_atomic_refusal() {
    let db=Database::load().await; let seller=pilot(&db).await; let buyer=pilot(&db).await;
    let volume_before=traded_volume(&db,35);
    let item=stock(&db,seller,60000004,35,10).await;
    let ask=run(&db,seller,order(60000004,35,3.0,10,false,1,Some(item))).await.ok().unwrap().orders[0].0;
    let bid=run(&db,buyer,order(60000004,35,2.0,5,true,1,None)).await.ok().unwrap().orders[0].0;
    assert_eq!(balance(&db,buyer).await,1e9-110.0);
    assert!(db.store.write(|tx|modify(&mut ItemTxn::new(tx),&mut Effects::default(),seller,ask,2.0,(None,0))).is_ok());
    let r=order_row(&db,bid,|o|(o.int("volRemaining").unwrap(),o.float("escrow").unwrap(),o.int("orderState").unwrap()));
    assert_eq!(r,(0,0.0,2));
    assert_eq!(order_row(&db,ask,|o|o.int("volRemaining").unwrap()),5);
    assert!(db.store.write(|tx|cancel(&mut ItemTxn::new(tx),&mut Effects::default(),buyer,ask,(None,0))).is_err());
    assert!(db.store.write(|tx|cancel(&mut ItemTxn::new(tx),&mut Effects::default(),seller,ask,(None,0))).is_ok());
    assert_eq!(held(&db,&[seller],35),5);
    let next=run(&db,buyer,order(60000004,36,2.0,7,true,1,None)).await.ok().unwrap().orders[0].0;
    update(&db,"market_orders",next,&[("issued",Cell::Int(1))]);
    let before=balance(&db,buyer).await;
    for _ in 0..2 {assert!(db.store.write(|tx|expire_orders(&mut ItemTxn::new(tx),&mut Effects::default())).is_ok());}
    assert_eq!(balance(&db,buyer).await,before+14.0,"expiry refunds once");
    let before=balance(&db,buyer).await;
    assert!(run(&db,buyer,order(60000004,36,1.0,100,true,0,None)).await.is_err());
    assert_eq!(balance(&db,buyer).await,before,"failed immediate order rolls back all effects");
    assert_eq!(traded_volume(&db,35)-volume_before,5,"one history trade, not two wallet sides");
}

#[tokio::test]
async fn market_competing_buyers_cannot_duplicate_stock() {
    let db=Database::load().await; let seller=pilot(&db).await; let a=pilot(&db).await; let b=pilot(&db).await;
    let item=stock(&db,seller,60000004,37,1).await;
    assert!(run(&db,seller,order(60000004,37,3.0,1,false,1,Some(item))).await.is_ok());
    let (a_result,b_result)=tokio::join!(run(&db,a,order(60000004,37,3.0,1,true,0,None)),run(&db,b,order(60000004,37,3.0,1,true,0,None)));
    assert_eq!(usize::from(a_result.is_ok())+usize::from(b_result.is_ok()),1);
    assert_eq!(held(&db,&[a,b],37),1);
}

#[tokio::test]
async fn market_margin_corporation_delivery_records_notifications_and_permissions() {
    let db=Database::load().await; let issuer=pilot(&db).await; let seller=pilot(&db).await;
    let corp=i64::from(db.id().await);
    db.store.write(|tx|tx.insert("corporation",&[("corporationID",Cell::Int(corp)),("corporationName",Cell::Text(format!("Market corporation {corp}").into())),
        ("balance",Cell::Float(1000000.0))])).unwrap();
    let skill=stock(&db,issuer,issuer,16597,1).await;
    update(&db,"entity",skill,&[("flag",Cell::Int(7)),("singleton",Cell::Int(1))]);
    db.store.write(|tx|tx.insert("entity_attributes",&[("itemID",Cell::Int(skill)),("attributeID",Cell::Int(280)),("valueInt",Cell::Int(5))])).unwrap();
    let mut bid=order(60000004,38,100.0,10,true,1,None); bid.corp=true; bid.range=32767;
    let order_id=db.store.write(|tx|{
        let mut txn=ItemTxn::new(tx); let mut fx=Effects::default();
        place(&mut txn,&mut fx,issuer,bid,(Some(corp),1))?;
        Ok::<_,Refusal>(fx.orders[0].0)
    }).unwrap();
    let escrow=order_row(&db,order_id,|o|o.float("escrow").unwrap());
    assert_eq!(escrow,237.31);
    let other_station=db.store.read(|d|{
        let stations=d.table("staStations");
        let region=stations.get_id(60000004).and_then(|s|s.int("regionID"));
        stations.rows().filter(|s|region.is_some() && s.int("regionID")==region).filter_map(|s|s.int("stationID")).filter(|id|*id!=60000004).min()
    }).unwrap();
    let item=stock(&db,seller,other_station,38,10).await;
    let (mut issuer_rx,mut seller_rx)={let mut w=db.world.lock();let a=w.connect(1);let b=w.connect(2);
        for (client,char_id,corp_id) in [(1,issuer,corp),(2,seller,1000168)] {
            w.set_presence(client,Some(crate::world::Presence{char_id,corp_id,solar_system_id:30000142,docked:true,station_id:Some(other_station),role:3}));
        } (a,b)};
    let fx=run(&db,seller,order(other_station,38,100.0,10,false,0,Some(item))).await.unwrap();
    let session=Session{char_id:Some(seller),..Default::default()}; tell(&db.ctx(&session),fx).await;
    let destination=db.store.read(|d|d.table("entity").find(&["ownerID","typeID"],&[Cell::Int(corp),Cell::Int(38)])
        .filter(|e|e.int("flag")==Some(62)).filter_map(|e|e.int("locationID")).next()).unwrap();
    assert_eq!(destination,other_station,"a ranged buy does not teleport purchased goods");
    let remaining=db.store.read(|d|d.table("corporation").get_id(corp).and_then(|c|c.float("balance"))).unwrap();
    assert!((remaining-998900.0).abs()<0.001,"escrow and later funding together pay exactly the trade plus fee");
    let r=db.store.read(|d|d.table("market_transactions").find(&["corporationID"],&[Cell::Int(corp)])
        .map(|t|(t.int("characterID"),t.int("isCorp"),t.int("stationID"))).next()).unwrap();
    assert_eq!(r.0,Some(issuer)); assert_eq!(r.1,Some(1)); assert_eq!(r.2,Some(other_station));
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
async fn market_rejects_forged_assets_insufficient_funds_bad_ranges_and_order_limits() {
    let db=Database::load().await; let me=pilot(&db).await;let stranger=pilot(&db).await;
    let foreign=stock(&db,stranger,60000004,39,1).await;
    assert!(run(&db,me,order(60000004,39,1.0,1,false,1,Some(foreign))).await.is_err());
    for price in [f64::NAN,f64::INFINITY,-1.0,0.0] {assert!(run(&db,me,order(60000004,39,price,1,true,1,None)).await.is_err());}
    for _ in 0..5 {run(&db,me,order(60000004,39,0.01,1,true,1,None)).await.unwrap();}
    let before=balance(&db,me).await;
    assert!(run(&db,me,order(60000004,39,0.01,1,true,1,None)).await.is_err());
    assert_eq!(balance(&db,me).await,before);
    update(&db,"character_",stranger,&[("balance",Cell::Float(0.0))]);
    assert!(run(&db,stranger,order(60014689,34,1e6,1,true,0,None)).await.is_err());
    let session=Session{char_id:Some(me),station_id:Some(60014689),solar_system_id2:Some(30004971),region_id:Some(10000064),..Default::default()};
    assert!(access(&db.ctx(&session),60014659,16594,true,None).await.is_err(),"different region");
    let other=db.store.read(|d|d.table("staStations").rows().filter(|s|s.int("regionID")==Some(10000064)).filter_map(|s|s.int("stationID")).find(|id|*id!=60014689)).unwrap();
    assert!(access(&db.ctx(&session),other,16594,false,None).await.is_err(),"untrained remote order");
    assert!(access(&db.ctx(&session),other,16594,true,None).await.is_ok(),"immediate remote purchase");
}

#[tokio::test]
async fn market_division_wallet_selection_funding_escrow_refund_and_journal_stay_separate() {
    let db=Database::load().await;let me=pilot(&db).await;let corp=i64::from(db.id().await);
    db.store.write(|tx|tx.insert("corporation",&[("corporationID",Cell::Int(corp)),("corporationName",Cell::Text(format!("Division market {corp}").into())),
        ("balance",Cell::Float(1000000.0))])).unwrap();
    let mut session=Session{char_id:Some(me),corp_id:Some(corp),roles_at_all:1,..Default::default()};
    let transfer=Call{service:"account".into(),method:"GiveCashFromCorpAccount".into(),
        args:vec![Value::Int(corp),Value::Double(10000.0),Value::Int(1000)],
        kwargs:vec![(Value::bytes("toAccountKey"),Value::Int(1002))]};
    assert!(matches!(crate::svc::wallet::account(&transfer,&db.ctx(&session)).await,Some(Outcome::Value(_))));
    crate::svc::wallet::select_account(&db.store,&mut session,1002).await.unwrap();
    assert_eq!(session.corp_account_key,Some(1002));
    let saved=db.store.read(|d|d.table("character_").get_id(me).and_then(|c|c.int("corpAccountKey"))).unwrap();assert_eq!(saved,1002);
    let mut bid=order(60000004,40,100.0,10,true,1,None);bid.corp=true;bid.account=1002;
    let id=db.store.write(|tx|{
        let mut txn=ItemTxn::new(tx);let mut fx=Effects::default();
        place(&mut txn,&mut fx,me,bid,(Some(corp),1))?;
        Ok::<_,Refusal>(fx.orders[0].0)
    }).unwrap();
    let division=|db:&Database|db.store.read(|d|d.table("corporation_wallets").get(&[Cell::Int(corp),Cell::Int(1002)]).and_then(|w|w.float("balance"))).unwrap();
    assert_eq!(division(&db),8900.0);
    db.store.write(|tx|cancel(&mut ItemTxn::new(tx),&mut Effects::default(),me,id,(Some(corp),1))).unwrap();
    assert_eq!(division(&db),9900.0);
    let amount=db.store.read(|d|d.table("corporation").get_id(corp).and_then(|c|c.float("balance"))).unwrap();assert_eq!(amount,990000.0);
    let n=db.store.read(|d|d.table("market_journal").find(&["characterID","accountKey"],&[Cell::Int(corp),Cell::Int(1002)]).count());assert_eq!(n,4);
    session.roles_at_all=0;
    assert!(crate::svc::wallet::select_account(&db.store,&mut session,1003).await.is_err());
    assert!(matches!(crate::svc::wallet::account(&transfer,&db.ctx(&session)).await,Some(Outcome::UserError{..})));
}
