//! Identities the Trinity data adds over the older item dump: an item for
//! every solar system and station, and a character for every agent. Rows
//! already there are kept as they are, player station ownership included.

use crate::store::{StoreError, Tx, Value};

pub fn ensure(tx: &mut Tx) -> Result<(), StoreError> {
    // Inventory lookups use entity, while the ballpark uses mapDenormalize.
    // Without these rows new systems/stations exist on the map but have no item.
    let systems: Vec<Vec<(&str, Value)>> = tx
        .table("mapSolarSystems")
        .rows()
        .map(|s| {
            vec![
                ("itemID", s.value("solarSystemID").clone()),
                ("itemName", s.value("solarSystemName").clone()),
                ("typeID", Value::Int(5)),
                ("ownerID", Value::Int(1)),
                ("locationID", Value::Int(0)),
                ("singleton", Value::Int(1)),
                ("quantity", Value::Int(1)),
                ("x", s.value("x").clone()),
                ("y", s.value("y").clone()),
                ("z", s.value("z").clone()),
            ]
        })
        .collect();
    let stations: Vec<Vec<(&str, Value)>> = tx
        .table("staStations")
        .rows()
        .map(|s| {
            vec![
                ("itemID", s.value("stationID").clone()),
                ("itemName", s.value("stationName").clone()),
                ("typeID", s.value("stationTypeID").clone()),
                ("ownerID", s.value("corporationID").clone()),
                ("locationID", s.value("solarSystemID").clone()),
                ("singleton", Value::Int(1)),
                ("quantity", Value::Int(1)),
                ("x", s.value("x").clone()),
                ("y", s.value("y").clone()),
                ("z", s.value("z").clone()),
            ]
        })
        .collect();
    for table in ["entityStatic", "entity"] {
        for row in systems.iter().chain(&stations) {
            if tx.table(table).get(std::slice::from_ref(&row[0].1)).is_none() {
                tx.insert_or_ignore(table, row)?;
            }
        }
    }
    // The export supplies names/types/affiliations and bloodlines, but no NPC
    // portrait appearance. Do not overwrite older character records with defaults.
    let bloodlines = crate::sde::table("chrBloodlines");
    let mut agents: Vec<Vec<(&str, Value)>> = Vec::new();
    for a in crate::sde::table("agtAgents").rows() {
        let Some(agent) = a.int("agentID") else { continue };
        let Some(owner) = tx.table("eveStaticOwners").get_id(agent) else { continue };
        let type_id = owner.value("typeID").clone();
        let bloodline = type_id
            .int()
            .and_then(|t| crate::sde::table("bloodlineTypes").get("typeID", t))
            .and_then(|bt| bt.int("bloodlineID"))
            .and_then(|b| bloodlines.get("bloodlineID", b));
        let station = a.int("stationID").and_then(|s| tx.table("staStations").get_id(s));
        let place = |c: &str| station.and_then(|s| s.int(c)).unwrap_or(0);
        agents.push(vec![
            ("characterID", Value::Int(agent)),
            ("characterName", owner.value("ownerName").clone()),
            ("corporationID", Value::Int(a.int("corporationID").unwrap_or(0))),
            ("typeID", type_id),
            ("bloodlineID", Value::Int(bloodline.and_then(|b| b.int("bloodlineID")).unwrap_or(0))),
            ("raceID", Value::Int(bloodline.and_then(|b| b.int("raceID")).unwrap_or(0))),
            ("stationID", Value::Int(a.int("stationID").unwrap_or(0))),
            ("solarSystemID", Value::Int(place("solarSystemID"))),
            ("constellationID", Value::Int(place("constellationID"))),
            ("regionID", Value::Int(place("regionID"))),
        ]);
    }
    for table in ["characterStatic", "character_"] {
        for row in &agents {
            if tx.table(table).get(std::slice::from_ref(&row[0].1)).is_none() {
                tx.insert_or_ignore(table, row)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_system_station_and_agent_has_its_identity_and_items_are_kept() {
        let store = crate::test_support::store();
        store.read(|db| {
            let entity = db.table("entity");
            assert!(db.table("staStations").rows().all(|s| entity.get(std::slice::from_ref(s.value("stationID"))).is_some()));
            assert!(db.table("mapSolarSystems").rows().all(|s| entity.get(std::slice::from_ref(s.value("solarSystemID"))).is_some()));
            let characters = db.table("character_");
            for a in crate::sde::table("agtAgents").rows() {
                let id = a.int("agentID").unwrap();
                if db.table("eveStaticOwners").get_id(id).is_some() {
                    assert!(characters.get_id(id).is_some(), "agent {id}");
                }
            }
        });
        let before = store.read(|db| (db.table("entity").len(), db.table("entity").rows().filter_map(|r| r.int("ownerID")).sum::<i64>()));
        store.write(super::ensure).unwrap();
        let after = store.read(|db| (db.table("entity").len(), db.table("entity").rows().filter_map(|r| r.int("ownerID")).sum::<i64>()));
        assert_eq!(before, after);
    }
}
