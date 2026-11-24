//! Actor `SQLite` row readers, text binders and the bot key every actor is addressed by.
use anyhow::{Result, bail};
use rivetkit::{Actor, ActorKeySegment, BindParam, ColumnValue, Ctx};

/// Reads a text column.
pub fn string(row: &[ColumnValue], index: usize) -> Result<String> {
    match row.get(index) {
        Some(ColumnValue::Text(value)) => Ok(value.clone()),
        _ => bail!("invalid stored text"),
    }
}

/// Reads an integer column.
pub fn integer(row: &[ColumnValue], index: usize) -> Result<i64> {
    match row.get(index) {
        Some(ColumnValue::Integer(value)) => Ok(*value),
        _ => bail!("invalid stored integer"),
    }
}

/// Binds text values in statement order.
pub fn params(values: &[&str]) -> Option<Vec<BindParam>> {
    Some(
        values
            .iter()
            .map(|value| BindParam::Text((*value).to_owned()))
            .collect(),
    )
}

/// The bot ID: the first segment of the actor key.
pub fn bot_key<A: Actor>(ctx: &Ctx<A>) -> Result<String> {
    match ctx.key().first() {
        Some(ActorKeySegment::String(key)) => Ok(key.clone()),
        _ => bail!("bot requires a string key"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readers_check_column_types() {
        let row = [
            ColumnValue::Text("turn".into()),
            ColumnValue::Integer(7),
            ColumnValue::Null,
        ];
        assert_eq!(string(&row, 0).unwrap(), "turn");
        assert_eq!(integer(&row, 1).unwrap(), 7);
        assert!(string(&row, 1).is_err());
        assert!(integer(&row, 0).is_err());
        assert!(string(&row, 2).is_err());
        assert!(integer(&row, 3).is_err());
    }

    #[test]
    fn params_bind_text_in_order() {
        let bound = params(&["a", "b"]).unwrap();
        assert!(matches!(
            bound.as_slice(),
            [BindParam::Text(a), BindParam::Text(b)] if a == "a" && b == "b"
        ));
    }
}
