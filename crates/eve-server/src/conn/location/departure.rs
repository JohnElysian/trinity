//! Revalidate departure permissions inside the arrival/payment transaction.
//! SQL endpoint locks last through COMMIT. Dynamic fleet/field authorization is
//! accepted under the world lock immediately before COMMIT, never before a fuel wait.
use super::*;
use crate::{
    services::Ctx,
    world::{Bridge, World, WorldState},
};
use tokio_postgres::Transaction;

#[cfg(test)]
mod tests;
pub(super) mod reserved;
pub(super) mod transition;
mod completion;

pub(super) struct Departure<'a> {
    catalog: &'a crate::dogma::Catalog,
    #[cfg(test)]
    world: World,
    request: JumpRequest,
    ship: Ship,
    from: i64,
    to: i64,
    bridge: Option<Bridge>,
    #[cfg(test)]
    pub(super) configuration: Option<crate::svc::maintenance::Configuration>,
}

impl<'a> Departure<'a> {
    pub(super) fn capture(
        ctx: &Ctx<'a>,
        w: &WorldState,
        request: JumpRequest,
        to: i64,
    ) -> std::result::Result<Self, String> {
        let from = ctx.session.solar_system_id.ok_or("You are not in space.")?;
        let ship = w
            .parks
            .get(&from)
            .and_then(|p| p.ships.get(&ctx.session.ship_id.unwrap_or(0)))
            .filter(|s| {
                s.is_pilot()
                    && s.client_id == ctx.client_id
                    && s.pilot.char_id == ctx.session.char_id.unwrap_or(0)
                    && !s.identity_reserved
                    && !s.warping()
                    && !s.cloaked
            })
            .ok_or("You no longer control that ship here.")?
            .clone();
        let bridge = match request {
            JumpRequest::Portal { ship, .. } => w.bridges.get(&ship).cloned(),
            _ => None,
        };
        #[cfg(test)]
        let configuration = w.maintenance.get(&ship.id).copied();
        let result = Self {
            catalog: ctx.dogma,
            #[cfg(test)]
            world: ctx.world.clone(),
            request,
            ship,
            from,
            to,
            bridge,
            #[cfg(test)]
            configuration,
        };
        result.live(w)?;
        Ok(result)
    }

    fn live(&self, w: &WorldState) -> std::result::Result<(), String> {
        let pilot = self.ship.pilot.char_id;
        // Identity reservation keeps combat and ordinary motion running. Use
        // the current hull for late range checks, not its capture-time position.
        let traveler=w.parks.get(&self.from).and_then(|p|p.ships.get(&self.ship.id))
            .filter(|s|s.pilot.char_id==pilot && s.client_id==self.ship.client_id).unwrap_or(&self.ship);
        match self.request {
            JumpRequest::Beacon { member, beacon } => {
                if w.gangs.of(pilot).is_none() || w.gangs.of(pilot) != w.gangs.of(member) {
                    return Err("The cynosural field's pilot is no longer in your gang.".into());
                }
                if crate::svc::jumps::field_at(w, self.to, beacon, member).is_none() {
                    return Err("The cynosural field has gone out.".into());
                }
                if w.parks[&self.to].beacons[&beacon].type_id
                    == crate::beacons::TYPE_COVERT_CYNOSURAL_FIELD
                    && traveler.pilot.group_id != 898
                {
                    return Err("Only Black Ops jump drives can use that covert field.".into());
                }
            }
            JumpRequest::Portal {
                pilot: host,
                ship: portal,
                alliance,
            } => {
                let b = self.bridge.as_ref().ok_or("The jump portal has closed.")?;
                if w.bridges.get(&portal) != Some(b)
                    || b.system != self.from
                    || b.to_system != self.to
                    || b.titan_pilot != host
                    || b.until_ms <= crate::combat::now_ms()
                    || !crate::svc::jumps::portal_is_live(w, portal, b)
                {
                    return Err("The jump portal changed before your departure.".into());
                }
                let source = &w.parks[&self.from].ships[&portal];
                if (traveler.pos - source.pos).len() - traveler.stats.radius - source.stats.radius
                    > crate::svc::jumps::BRIDGE_RANGE
                {
                    return Err("The jump portal moved out of range.".into());
                }
                if alliance {
                    if b.member.is_some()
                        || traveler.pilot.alliance_id.filter(|id| *id > 0).is_none()
                        || traveler.pilot.alliance_id != source.pilot.alliance_id
                    {
                        return Err("You are no longer in the jump portal's alliance.".into());
                    }
                } else if w.gangs.of(pilot).is_none() || w.gangs.of(pilot) != w.gangs.of(host) {
                    return Err("You are no longer in the jump portal's gang.".into());
                }
            }
            JumpRequest::Structure { local, .. } => {
                let source = w
                    .parks
                    .get(&self.from)
                    .and_then(|p| p.structures.get(&local))
                    .ok_or("The jump array is no longer here.")?;
                if !matches!(source.state, 4 | 5)
                    || (traveler.pos - source.pos).len() - traveler.stats.radius - source.radius
                        > crate::svc::jumps::BRIDGE_RANGE
                {
                    return Err("The jump array is no longer available within range.".into());
                }
            }
            JumpRequest::Alliance { .. } => {}
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn accept(&self) -> std::result::Result<(), String> {
        self.live(&self.world.lock())
    }

    pub(super) async fn lock_endpoints(
        &self,
        tx: &Transaction<'_>,
    ) -> std::result::Result<(), String> {
        let pilot = self.ship.pilot.char_id as i32;
        let corp = tx
            .query_one(
                r#"SELECT "corporationID" FROM "character_" WHERE "characterID"=$1"#,
                &[&pilot],
            )
            .await
            .map_err(travel_failed)?
            .get::<_, i32>(0);
        match self.request {
            JumpRequest::Alliance { beacon } => self.fixed_beacon(tx, corp, beacon).await?,
            JumpRequest::Portal {
                pilot: host,
                ship: portal,
                alliance,
            } => {
                // Match the host's own transition lock order: character first,
                // then hull, fitted generator and fuel.
                let host_corp=tx.query_opt(r#"SELECT "corporationID" FROM "character_" WHERE "characterID"=$1 FOR SHARE"#,&[&(host as i32)]).await.map_err(travel_failed)?
                    .ok_or("The jump portal's pilot is unavailable.")?.get::<_,i32>(0);
                let row = tx
                    .query_opt(
                        r#"SELECT "itemID" FROM entity WHERE "itemID"=$1 AND "ownerID"=$2
                    AND "locationID"=$3 AND COALESCE(flag,0)=0 AND quantity>0 FOR SHARE"#,
                        &[&(portal as i32), &(host as i32), &(self.from as i32)],
                    )
                    .await
                    .map_err(travel_failed)?;
                if row.is_none() {
                    return Err("The jump portal's ship has moved.".into());
                }
                let generator = self
                    .bridge
                    .as_ref()
                    .ok_or("The jump portal closed.")?
                    .generator_id;
                let fitted=tx.query_opt(r#"SELECT 1 FROM entity e JOIN entity_attributes a ON a."itemID"=e."itemID" AND a."attributeID"=2
                    WHERE e."itemID"=$1 AND e."ownerID"=$2 AND e."locationID"=$3 AND e.flag BETWEEN 11 AND 35
                    AND e.quantity>0 AND e.singleton<>0 AND COALESCE(a."valueFloat",a."valueInt"::float8,0)<>0 FOR SHARE OF e,a"#,
                    &[&(generator as i32),&(host as i32),&(portal as i32)]).await.map_err(travel_failed)?;
                if fitted.is_none() {
                    return Err("The jump portal generator is no longer fitted and online.".into());
                }
                let harmonics = self.bridge.as_ref().unwrap().harmonics;
                if harmonics > 0 {
                    let loadout = crate::dogma::Loadout::load(
                        tx,
                        self.catalog,
                        self.ship.pilot.char_id,
                        Some(self.ship.id),
                        &Default::default(),
                    )
                    .await
                    .map_err(travel_failed)?;
                    let index = loadout
                        .find(self.ship.id)
                        .ok_or("Your ship could not be read.")?;
                    if loadout.value(index, 1253) < harmonics as f64 {
                        return Err(
                            "Your ship's jump harmonics no longer permit this portal.".into()
                        );
                    }
                }
                if alliance {
                    let members = affiliations(tx, &[corp, host_corp]).await?;
                    let own = members.get(&corp).copied().flatten().filter(|id| *id > 0);
                    if own.is_none() || own != members.get(&host_corp).copied().flatten() {
                        return Err("The jump portal's alliance changed.".into());
                    }
                }
                if let Some(b) = self.bridge.as_ref().filter(|b| b.member.is_none()) {
                    self.fixed_beacon(tx, corp, b.beacon).await?;
                }
            }
            JumpRequest::Structure { local, remote } => {
                let link=tx.query_opt(r#"SELECT 1 FROM "jumpBridgeLinks" WHERE "sourceID"=$1 AND "destinationID"=$2 FOR SHARE"#,
                    &[&(local as i32),&(remote as i32)]).await.map_err(travel_failed)?;
                if link.is_none() {
                    return Err("The jump array link has changed.".into());
                }
                let ids = vec![local as i32, remote as i32];
                let rows = tx
                    .query(
                        r#"SELECT e."itemID",e."ownerID",e."locationID",t."groupID",e.quantity
                    FROM entity e JOIN "invTypes" t ON t."typeID"=e."typeID"
                    WHERE e."itemID"=ANY($1) ORDER BY e."itemID" FOR SHARE OF e"#,
                        &[&ids],
                    )
                    .await
                    .map_err(travel_failed)?;
                if rows.len() != 2
                    || rows.iter().any(|r| {
                        r.get::<_, i32>(3) != 707
                            || r.get::<_, i32>(4) <= 0
                            || i64::from(r.get::<_, i32>(2))
                                != if i64::from(r.get::<_, i32>(0)) == local {
                                    self.from
                                } else {
                                    self.to
                                }
                    })
                {
                    return Err("The jump array endpoints have changed.".into());
                }
                let states=tx.query(r#"SELECT state FROM "posStructures" WHERE "itemID"=ANY($1) ORDER BY "itemID" FOR SHARE"#,&[&ids]).await.map_err(travel_failed)?;
                if states.len() != 2 || states.iter().any(|r| !matches!(r.get::<_, i16>(0), 4 | 5))
                {
                    return Err("Both jump arrays must remain online.".into());
                }
                let owner = rows
                    .iter()
                    .find(|r| i64::from(r.get::<_, i32>(0)) == local)
                    .unwrap()
                    .get::<_, i32>(1);
                if rows.iter().any(|r| r.get::<_, i32>(1) != owner) {
                    return Err(
                        "The linked jump arrays no longer belong to the same corporation.".into(),
                    );
                }
                if corp != owner {
                    let members = affiliations(tx, &[corp, owner]).await?;
                    let own = members.get(&corp).copied().flatten().filter(|id| *id > 0);
                    if own.is_none() || own != members.get(&owner).copied().flatten() {
                        return Err("The jump array's alliance changed.".into());
                    }
                }
            }
            JumpRequest::Beacon { .. } => {}
        }
        Ok(())
    }

    async fn fixed_beacon(
        &self,
        tx: &Transaction<'_>,
        corp: i32,
        beacon: i64,
    ) -> std::result::Result<(), String> {
        let row=tx.query_opt(r#"SELECT e."ownerID" FROM entity e JOIN "invTypes" t ON t."typeID"=e."typeID"
            WHERE e."itemID"=$1 AND e."locationID"=$2 AND t."groupID"=838 AND e.quantity>0 FOR SHARE OF e"#,
            &[&(beacon as i32),&(self.to as i32)]).await.map_err(travel_failed)?.ok_or("The alliance beacon has moved or disappeared.")?;
        let state = tx
            .query_opt(
                r#"SELECT state FROM "posStructures" WHERE "itemID"=$1 FOR SHARE"#,
                &[&(beacon as i32)],
            )
            .await
            .map_err(travel_failed)?;
        if state.is_none_or(|r| !matches!(r.get::<_, i16>(0), 4 | 5)) {
            return Err("The alliance beacon is no longer online.".into());
        }
        let owner = row.get::<_, i32>(0);
        let members = affiliations(tx, &[corp, owner]).await?;
        let own = members.get(&corp).copied().flatten().filter(|id| *id > 0);
        if own.is_none() || own != members.get(&owner).copied().flatten() {
            return Err("The beacon no longer belongs to your alliance.".into());
        }
        Ok(())
    }
}

async fn affiliations(
    tx: &Transaction<'_>,
    corps: &[i32],
) -> std::result::Result<std::collections::BTreeMap<i32, Option<i32>>, String> {
    let rows=tx.query(r#"SELECT "corporationID","allianceID" FROM corporation WHERE "corporationID"=ANY($1) ORDER BY "corporationID" FOR SHARE"#,&[&corps]).await.map_err(travel_failed)?;
    Ok(rows.into_iter().map(|r| (r.get(0), r.get(1))).collect())
}
