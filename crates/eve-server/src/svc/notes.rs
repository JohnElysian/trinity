use eve_marshal::Value;
use tracing::error;

use crate::services::{Call, Ctx, Outcome};
use crate::store::{StoreError, Value as Cell};
use crate::svc::rows::rowset;

pub async fn character(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let arg = |i: usize| call.arg_i64(i);
    let value = match call.method.as_str() {
        "GetOwnerNoteLabels" => owner_note_labels(ctx).await,
        "GetOwnerNote" => owner_note(ctx, arg(0).unwrap_or(0)).await,
        "AddOwnerNote" => {
            add_owner_note(ctx, call.arg_text(0), call.arg_text(1)).await
        }
        "EditOwnerNote" => {
            edit_owner_note(ctx, arg(0).unwrap_or(0), call.arg_text(1), call.arg_text(2)).await
        }
        "RemoveOwnerNote" => remove_owner_note(ctx, arg(0).unwrap_or(0)).await,

        "GetNote" => get_note(ctx, arg(0).unwrap_or(0)).await,
        "SetNote" => set_note(ctx, arg(0).unwrap_or(0), call.arg_text(1)).await,

        _ => return None,
    };
    Some(Outcome::Value(value))
}

async fn owner_note_labels(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &["noteID", "label"];
    let Some(char_id) = ctx.session.char_id else { return rowset(HEADER, Vec::new()) };
    let lines = ctx.store.read(|db| {
        let mut notes: Vec<_> =
            db.table("chrOwnerNote").find(&["ownerID"], &[Cell::Int(char_id)]).filter(|n| n.text("label") != Some("")).collect();
        notes.sort_by_key(|n| n.int("noteID"));
        notes.iter().map(|n| Value::List(n.line(HEADER))).collect()
    });
    rowset(HEADER, lines)
}

async fn owner_note(ctx: &Ctx<'_>, note_id: i64) -> Value {
    const HEADER: &[&str] = &["noteID", "label", "note"];
    let Some(char_id) = ctx.session.char_id else { return rowset(HEADER, Vec::new()) };
    let lines = ctx.store.read(|db| {
        db.table("chrOwnerNote")
            .get_id(note_id)
            .filter(|n| n.int("ownerID") == Some(char_id))
            .map(|n| Value::List(n.line(HEADER)))
            .into_iter()
            .collect()
    });
    rowset(HEADER, lines)
}

async fn add_owner_note(ctx: &Ctx<'_>, label: Option<String>, note: Option<String>) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::None };
    let label = label.unwrap_or_default();
    let note = note.unwrap_or_default();
    let added = ctx.store.write(|tx| -> Result<_, StoreError> {
        let id = tx.nextval("chrOwnerNote_noteID_seq")?;
        let row = tx.insert(
            "chrOwnerNote",
            &[
                ("noteID", Cell::Int(id)),
                ("ownerID", Cell::Int(char_id)),
                ("label", Cell::Text(label.as_str().into())),
                ("note", Cell::Text(note.as_str().into())),
            ],
        )?;
        Ok(tx.table("chrOwnerNote").row(row).map_or(Value::None, |n| n.cell("noteID")))
    });
    match added {
        Ok(id) => id,
        Err(e) => {
            error!("AddOwnerNote({label:?}): {err}", err = crate::logging::chain(&e));
            Value::None
        }
    }
}

async fn edit_owner_note(
    ctx: &Ctx<'_>,
    note_id: i64,
    label: Option<String>,
    note: Option<String>,
) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::None };
    let label = label.unwrap_or_default();
    let result = ctx.store.write(|tx| {
        let Some(row) = tx.table("chrOwnerNote").get_id(note_id).filter(|n| n.int("ownerID") == Some(char_id)).map(|n| n.id) else {
            return Ok(());
        };
        match note {
            Some(note) => tx.update("chrOwnerNote", row, &[("label", Cell::Text(label.into())), ("note", Cell::Text(note.into()))]),
            None => tx.update("chrOwnerNote", row, &[("label", Cell::Text(label.into()))]),
        }
    });
    if let Err(e) = result {
        error!("EditOwnerNote({note_id}): {err}", err = crate::logging::chain(&e));
    }
    Value::None
}

async fn remove_owner_note(ctx: &Ctx<'_>, note_id: i64) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::None };
    let result = ctx.store.write(|tx| {
        match tx.table("chrOwnerNote").get_id(note_id).filter(|n| n.int("ownerID") == Some(char_id)).map(|n| n.id) {
            Some(row) => tx.delete("chrOwnerNote", row),
            None => Ok(()),
        }
    });
    if let Err(e) = result {
        error!("RemoveOwnerNote({note_id}): {err}", err = crate::logging::chain(&e));
    }
    Value::None
}

async fn get_note(ctx: &Ctx<'_>, item_id: i64) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::bytes("") };
    ctx.store.read(|db| {
        db.table("chrNotes").get(&[Cell::Int(char_id), Cell::Int(item_id)]).map_or_else(|| Value::bytes(""), |n| n.cell("note"))
    })
}

async fn set_note(ctx: &Ctx<'_>, item_id: i64, text: Option<String>) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::None };
    let text = Cell::Text(text.unwrap_or_default().into());
    let result = ctx.store.write(|tx| {
        tx.upsert(
            "chrNotes",
            &["characterID", "itemID"],
            &[("characterID", Cell::Int(char_id)), ("itemID", Cell::Int(item_id)), ("note", text.clone())],
            &[("note", text.clone())],
        )
    });
    if let Err(e) = result {
        error!("SetNote({item_id}): {err}", err = crate::logging::chain(&e));
    }
    Value::None
}
