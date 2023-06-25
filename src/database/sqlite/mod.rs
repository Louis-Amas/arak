mod keywords;

use crate::database::{
    self,
    event_visitor::{self, VisitKind, VisitValue},
    Database, Log,
};
use anyhow::{anyhow, Context, Result};
use rusqlite::{
    types::{ToSqlOutput, Type as SqlType, Value as SqlValue, ValueRef as SqlValueRef},
    Connection, OpenFlags, Transaction,
};
use solabi::{
    abi::EventDescriptor,
    value::{Value as AbiValue, ValueKind as AbiKind},
};
use std::{
    collections::{HashMap, HashSet},
    env,
    fmt::Write,
};
use url::Url;

pub struct Sqlite {
    connection: Connection,
    inner: SqliteInner,
}

impl Sqlite {
    pub fn new(connection: Connection) -> Result<Self> {
        let inner = SqliteInner::new(&connection)?;
        Ok(Self { connection, inner })
    }

    /// Opens a new SQLite database backend for the specified URL. The expected
    /// URL format is `sqlite://[/path[?query]]`. For example:
    ///
    /// - `sqlite://` to open and in-memory connection
    /// - `sqlite:///relative/foo.db` to open the file `relative/foo.db`
    /// - `sqlite:////absolute/foo.db` to open the file `/absolute/foo.db`
    ///
    /// Addionally, query string parameters can be set to configure database
    /// connection options. See <https://www.sqlite.org/uri.html> for supported
    /// query string paramters.
    pub fn open(url: &Url) -> Result<Self> {
        anyhow::ensure!(url.scheme() == "sqlite", "not an sqlite:// URL");
        anyhow::ensure!(
            url.has_authority() && url.authority() == "",
            "sqlite:// URL requires empty authority"
        );
        anyhow::ensure!(
            url.fragment().is_none(),
            "sqlite:// URL does not support fragments"
        );

        if url.path().is_empty() {
            tracing::debug!("opening in-memory database");
            return Self::new(Connection::open_in_memory()?);
        };

        // SQLite 3 supports connection strings as file:// URLs, convert our
        // `sqlite://` to that.
        let path = env::current_dir()?.join(
            url.path()
                .strip_prefix('/')
                .expect("can-be-a-base URL not prefixed with /"),
        );
        let mut file = Url::from_file_path(path)
            .ok()
            .context("invalid sqlite:// URL file path")?;
        if let Some(query) = url.query() {
            file.set_query(Some(query));
        }

        tracing::debug!("opening database {file}");
        let connection = Connection::open_with_flags(
            file.as_str(),
            OpenFlags::default() | OpenFlags::SQLITE_OPEN_URI,
        )?;

        Self::new(connection)
    }

    #[cfg(test)]
    /// Create a temporary in memory database for tests.
    pub fn new_for_test() -> Self {
        Self::new(Connection::open_in_memory().unwrap()).unwrap()
    }
}

impl Database for Sqlite {
    fn prepare_event(&mut self, name: &str, event: &EventDescriptor) -> Result<()> {
        let transaction = self.connection.transaction().context("transaction")?;
        self.inner.prepare_event(&transaction, name, event)?;
        transaction.commit().context("commit")
    }

    fn event_block(&mut self, name: &str) -> Result<database::Block> {
        self.inner.event_block(&self.connection, name)
    }

    fn update(&mut self, blocks: &[database::EventBlock], logs: &[database::Log]) -> Result<()> {
        let transaction = self.connection.transaction().context("transaction")?;
        self.inner.update(&transaction, blocks, logs)?;
        transaction.commit().context("commit")
    }

    fn remove(&mut self, uncles: &[database::Uncle]) -> Result<()> {
        let transaction = self.connection.transaction().context("transaction")?;
        self.inner.remove(&transaction, uncles)?;
        transaction.commit().context("commit")
    }
}

/// Columns that every event table has.
const FIXED_COLUMNS: &str = "block_number INTEGER NOT NULL, log_index INTEGER NOT NULL, transaction_index INTEGER NOT NULL, address BLOB NOT NULL";
const FIXED_COLUMNS_COUNT: usize = 4;
const PRIMARY_KEY: &str = "block_number ASC, log_index ASC";

/// Column for array tables.
const ARRAY_COLUMN: &str = "array_index INTEGER NOT NULL";
const PRIMARY_KEY_ARRAY: &str = "block_number ASC, log_index ASC, array_index ASC";

const CREATE_EVENT_BLOCK_TABLE: &str = "CREATE TABLE IF NOT EXISTS event_block(event TEXT PRIMARY KEY NOT NULL, indexed INTEGER NOT NULL, finalized INTEGER NOT NULL) STRICT;";
const GET_EVENT_BLOCK: &str = "SELECT indexed, finalized FROM event_block WHERE event = ?1;";
const NEW_EVENT_BLOCK: &str =
    "INSERT INTO event_block (event, indexed, finalized) VALUES(?1, 0, 0) ON CONFLICT(event) DO NOTHING;";
const SET_EVENT_BLOCK: &str =
    "UPDATE event_block SET indexed = ?2, finalized = ?3 WHERE event = ?1;";
const SET_INDEXED_BLOCK: &str = "UPDATE event_block SET indexed = ?2 WHERE event = ?1";

// Separate type because of lifetime issues when creating transactions. Outer struct only stores the connection itself.
struct SqliteInner {
    /// Invariant: Events in the map have corresponding tables in the database.
    events: HashMap<String, PreparedEvent>,
}

/// An event is represented in the database in several tables.
///
/// All tables have some columns that are unrelated to the event's fields. See `FIXED_COLUMNS`. The first table contains all fields that exist once per event which means they do not show up in arrays. The other tables contain fields that are part of arrays. Those tables additionally have the column `ARRAY_COLUMN`.
///
/// The order of tables and fields is given by the `event_visitor` module.
struct PreparedEvent {
    descriptor: EventDescriptor,
    insert_statements: Vec<InsertStatement>,
    /// Prepared statements for removing rows starting at some block number.
    /// Every statement takes a block number as parameter.
    remove_statements: Vec<String>,
}

/// Parameters:
/// - 1: block number
/// - 2: log index
/// - 3: array index if this is an array table (all tables after the first)
/// - 3 + n: n-th event field/column
#[derive(Debug)]
struct InsertStatement {
    sql: String,
    /// Number of event fields that map to SQL columns. Does not count FIXED_COLUMNS and array index.
    fields: usize,
}

impl SqliteInner {
    fn new(connection: &Connection) -> Result<Self> {
        connection
            .execute(CREATE_EVENT_BLOCK_TABLE, ())
            .context("create event_block table")?;

        connection
            .prepare_cached(GET_EVENT_BLOCK)
            .context("prepare get_event_block")?;
        connection
            .prepare_cached(SET_EVENT_BLOCK)
            .context("prepare set_event_block")?;
        connection
            .prepare_cached(SET_INDEXED_BLOCK)
            .context("prepare set_indexed_block")?;

        Ok(Self {
            events: Default::default(),
        })
    }

    fn sanitize_name(name: &str) -> String {
        let mut result: String = name
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if result.is_empty() || !result.chars().next().unwrap().is_ascii_alphabetic() {
            result.insert(0, '_');
        }
        let lowercase = result.to_ascii_lowercase();
        if keywords::SQLITE_KEYWORDS
            .iter()
            .any(|word| *word == lowercase)
        {
            result.push('_');
        }
        result
    }

    /*
        fn read_event(
            &self,
            c: &Connection,
            name: &str,
            block_number: u64,
            log_index: u64,
        ) -> Result<Vec<AbiValue>> {
            let name = Self::internal_event_name(name);
            let event = self.events.get(&name).context("unknown event")?;

            todo!()
        }
    */

    fn event_block(&self, con: &Connection, name: &str) -> Result<database::Block> {
        let name = Self::sanitize_name(name);
        let mut statement = con
            .prepare_cached(GET_EVENT_BLOCK)
            .context("prepare_cached")?;
        let block: (i64, i64) = statement
            .query_row((name,), |row| Ok((row.get(0)?, row.get(1)?)))
            .context("query_row")?;
        Ok(database::Block {
            indexed: block.0.try_into().context("indexed out of bounds")?,
            finalized: block.1.try_into().context("finalized out of bounds")?,
        })
    }

    fn set_event_blocks(&self, con: &Transaction, blocks: &[database::EventBlock]) -> Result<()> {
        let mut statement = con
            .prepare_cached(SET_EVENT_BLOCK)
            .context("prepare_cached")?;
        for block in blocks {
            let name = Self::sanitize_name(block.event);
            if !self.events.contains_key(&name) {
                return Err(anyhow!("event {name} wasn't prepared"));
            }
            let indexed: i64 = block
                .block
                .indexed
                .try_into()
                .context("indexed out of bounds")?;
            let finalized: i64 = block
                .block
                .finalized
                .try_into()
                .context("finalized out of bounds")?;
            let rows = statement
                .execute((name, indexed, finalized))
                .context("execute")?;
            if rows != 1 {
                return Err(anyhow!(
                    "query unexpectedly changed {rows} rows instead of 1"
                ));
            }
        }
        Ok(())
    }

    fn prepare_event(
        &mut self,
        con: &Transaction,
        name: &str,
        event: &EventDescriptor,
    ) -> Result<()> {
        let name = Self::sanitize_name(name);

        let mut column_names = Vec::<String>::new();
        let mut column_names_ = HashSet::<String>::new();
        for input in &event.inputs {
            let name = Self::sanitize_name(&input.field.name);
            if column_names_.contains(&name) {
                return Err(anyhow!("duplicate field name {:?}", name));
            }
            column_names.push(name.clone());
            column_names_.insert(name);
        }

        if let Some(existing) = self.events.get(&name) {
            if event != &existing.descriptor {
                return Err(anyhow!(
                    "event {name} already exists with different signature"
                ));
            }
            return Ok(());
        }

        // TODO:
        // - Check that either no table exists or all tables exist and with the right types.
        // - Maybe have `CHECK` clauses to enforce things like address and integers having expected length.

        let tables = event_to_tables(event).context("unsupported event")?;
        for (i, table) in tables.iter().enumerate() {
            let mut sql = String::new();
            write!(&mut sql, "CREATE TABLE IF NOT EXISTS {name}_{i} (").unwrap();
            write!(&mut sql, "{FIXED_COLUMNS}, ").unwrap();
            if i != 0 {
                write!(&mut sql, "{ARRAY_COLUMN}, ").unwrap();
            }
            for (j, column) in table.0.iter().enumerate() {
                // TODO: If The length of the vectors is different then there are top level values with tuples. Current code doesn't handle tuples.
                if i == 0 && column_names.len() == table.0.len() {
                    write!(&mut sql, "{}", &column_names[j]).unwrap();
                } else {
                    write!(&mut sql, "c{j}").unwrap();
                };
                let type_ = match column.0 {
                    SqlType::Null => unreachable!(),
                    SqlType::Integer => "INTEGER",
                    SqlType::Real => "REAL",
                    SqlType::Text => "TEXT",
                    SqlType::Blob => "BLOB",
                };
                write!(&mut sql, " {type_}, ").unwrap();
            }
            let primary_key = if i == 0 {
                PRIMARY_KEY
            } else {
                PRIMARY_KEY_ARRAY
            };
            write!(&mut sql, "PRIMARY KEY({primary_key})) STRICT;").unwrap();
            tracing::debug!("creating table:\n{}", sql);
            con.execute(&sql, ()).context("execute create_table")?;
        }

        let mut new_event_block = con
            .prepare_cached(NEW_EVENT_BLOCK)
            .context("prepare new_event_block")?;
        new_event_block
            .execute((&name,))
            .context("execute new_event_block")?;

        let insert_statements: Vec<InsertStatement> = tables
            .iter()
            .enumerate()
            .map(|(i, table)| {
                let is_array = i != 0;
                let mut sql = String::new();
                write!(&mut sql, "INSERT INTO {name}_{i} VALUES(").unwrap();
                for i in 0..table.0.len() + FIXED_COLUMNS_COUNT + is_array as usize {
                    write!(&mut sql, "?{},", i + 1).unwrap();
                }
                assert_eq!(sql.pop(), Some(','));
                write!(&mut sql, ");").unwrap();
                tracing::debug!("creating insert statement:\n{}", sql);
                InsertStatement {
                    sql,
                    fields: table.0.len(),
                }
            })
            .collect();

        let remove_statements: Vec<String> = (0..tables.len())
            .map(|i| format!("DELETE FROM {name}_{i} WHERE block_number >= ?1;"))
            .collect();

        // Check that prepared statements are valid. Unfortunately we can't distinguish the statement being wrong from other Sqlite errors like being unable to access the database file on disk.
        for statement in &insert_statements {
            con.prepare_cached(&statement.sql)
                .context("invalid prepared insert statement")?;
        }
        for statement in &remove_statements {
            con.prepare_cached(statement)
                .context("invalid prepared remove statement")?;
        }

        self.events.insert(
            name,
            PreparedEvent {
                descriptor: event.clone(),
                insert_statements,
                remove_statements,
            },
        );

        Ok(())
    }

    fn store_event<'a>(
        &self,
        con: &Transaction,
        Log {
            event,
            block_number,
            log_index,
            transaction_index,
            address,
            fields,
        }: &'a Log,
    ) -> Result<()> {
        let name = Self::sanitize_name(event);
        let event = self.events.get(&name).context("unknown event")?;

        let len = fields.len();
        let expected_len = event.descriptor.inputs.len();
        if fields.len() != expected_len {
            return Err(anyhow!(
                "event value has {len} fields but should have {expected_len}"
            ));
        }
        for (i, (value, kind)) in fields.iter().zip(&event.descriptor.inputs).enumerate() {
            if value.kind() != kind.field.kind {
                return Err(anyhow!("event field {i} doesn't match event descriptor"));
            }
        }

        // Outer vec maps to tables. Inner vec maps to (array element count, columns).
        let mut sql_values: Vec<(Option<usize>, Vec<ToSqlOutput<'a>>)> = vec![(None, vec![])];
        let mut in_array: bool = false;
        let mut visitor = |value: VisitValue<'a>| {
            let sql_value = match value {
                VisitValue::ArrayStart(len) => {
                    sql_values.push((Some(len), Vec::new()));
                    in_array = true;
                    return;
                }
                VisitValue::ArrayEnd => {
                    in_array = false;
                    return;
                }
                VisitValue::Value(AbiValue::Int(v)) => {
                    ToSqlOutput::Owned(SqlValue::Blob(v.get().to_be_bytes().to_vec()))
                }
                VisitValue::Value(AbiValue::Uint(v)) => {
                    ToSqlOutput::Owned(SqlValue::Blob(v.get().to_be_bytes().to_vec()))
                }
                VisitValue::Value(AbiValue::Address(v)) => {
                    ToSqlOutput::Borrowed(SqlValueRef::Blob(&v.0))
                }
                VisitValue::Value(AbiValue::Bool(v)) => {
                    ToSqlOutput::Owned(SqlValue::Integer(*v as i64))
                }
                VisitValue::Value(AbiValue::FixedBytes(v)) => {
                    ToSqlOutput::Borrowed(SqlValueRef::Blob(v.as_bytes()))
                }
                VisitValue::Value(AbiValue::Function(v)) => ToSqlOutput::Owned(SqlValue::Blob(
                    v.address
                        .0
                        .iter()
                        .copied()
                        .chain(v.selector.0.iter().copied())
                        .collect(),
                )),
                VisitValue::Value(AbiValue::Bytes(v)) => {
                    ToSqlOutput::Borrowed(SqlValueRef::Blob(v))
                }
                VisitValue::Value(AbiValue::String(v)) => {
                    ToSqlOutput::Borrowed(SqlValueRef::Blob(v.as_bytes()))
                }
                _ => unreachable!(),
            };
            (if in_array {
                <[_]>::last_mut
            } else {
                <[_]>::first_mut
            })(&mut sql_values)
            .unwrap()
            .1
            .push(sql_value);
        };
        for value in fields {
            event_visitor::visit_value(value, &mut visitor)
        }

        let block_number =
            ToSqlOutput::Owned(SqlValue::Integer((*block_number).try_into().unwrap()));
        let log_index = ToSqlOutput::Owned(SqlValue::Integer((*log_index).try_into().unwrap()));
        let transaction_index =
            ToSqlOutput::Owned(SqlValue::Integer((*transaction_index).try_into().unwrap()));
        let address = ToSqlOutput::Borrowed(SqlValueRef::Blob(&address.0));
        for (statement, (array_element_count, values)) in
            event.insert_statements.iter().zip(sql_values)
        {
            let mut statement_ = con
                .prepare_cached(&statement.sql)
                .context("prepare_cached")?;
            let is_array = array_element_count.is_some();
            let array_element_count = array_element_count.unwrap_or(1);
            assert_eq!(statement.fields * array_element_count, values.len());
            for i in 0..array_element_count {
                let row = &values[i * statement.fields..][..statement.fields];
                let array_index = if is_array {
                    Some(ToSqlOutput::Owned(SqlValue::Integer(i.try_into().unwrap())))
                } else {
                    None
                };
                let params = rusqlite::params_from_iter(
                    [&block_number, &log_index, &transaction_index, &address]
                        .into_iter()
                        .chain(array_index.as_ref())
                        .chain(row),
                );
                statement_.insert(params).context("insert")?;
            }
        }

        Ok(())
    }

    fn update(
        &self,
        con: &Transaction,
        blocks: &[database::EventBlock],
        logs: &[database::Log],
    ) -> Result<()> {
        self.set_event_blocks(con, blocks)
            .context("set_event_blocks")?;
        for log in logs {
            self.store_event(con, log).context("store_event")?;
        }
        Ok(())
    }

    fn remove(&self, connection: &Connection, uncles: &[database::Uncle]) -> Result<()> {
        let mut set_indexed_block = connection
            .prepare_cached(SET_INDEXED_BLOCK)
            .context("prepare_cached set_indexed_block")?;
        for uncle in uncles {
            let name = Self::sanitize_name(uncle.event);
            if uncle.number == 0 {
                return Err(anyhow!("block 0 got uncled"));
            }
            let block = i64::try_from(uncle.number).context("block out of bounds")?;
            let parent_block = block - 1;
            let prepared = self.events.get(&name).context("unprepared event")?;
            for remove_statement in &prepared.remove_statements {
                let mut remove_statement = connection
                    .prepare_cached(remove_statement)
                    .context("prepare_cached remove_statement")?;
                remove_statement
                    .execute((block,))
                    .context("execute remove_statement")?;
                set_indexed_block
                    .execute((&name, parent_block))
                    .context("execute set_indexed_block")?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Eq, PartialEq)]
struct Table(Vec<Column>);

#[derive(Debug, Eq, PartialEq)]
struct Column(SqlType);

fn event_to_tables(event: &EventDescriptor) -> Result<Vec<Table>> {
    // TODO:
    // - Handle indexed fields.
    // - Make use of field names and potentially tuple names.

    let values = event.inputs.iter().map(|input| &input.field.kind);

    // Nested dynamic arrays are rare and hard to handle. The recursive visiting code and SQL schema becomes more complicated. Handle this properly later.
    for value in values.clone() {
        if has_nested_dynamic_arrays(value) {
            return Err(anyhow!("nested dynamic arrays"));
        }
    }

    let mut tables = vec![Table(vec![])];
    for value in values {
        map_value(&mut tables, value);
    }

    Ok(tables)
}

fn has_nested_dynamic_arrays(value: &AbiKind) -> bool {
    let mut level: u32 = 0;
    let mut max_level: u32 = 0;
    let mut visitor = |visit: VisitKind| match visit {
        VisitKind::ArrayStart => {
            level += 1;
            max_level = std::cmp::max(max_level, level);
        }
        VisitKind::ArrayEnd => level -= 1,
        VisitKind::Value(_) => (),
    };
    event_visitor::visit_kind(value, &mut visitor);
    max_level > 1
}

fn map_value(tables: &mut Vec<Table>, value: &AbiKind) {
    assert!(!tables.is_empty());
    let mut table_index = 0;
    let mut visitor = move |value: VisitKind| {
        let type_ = match value {
            VisitKind::Value(&AbiKind::Int(_)) => SqlType::Blob,
            VisitKind::Value(&AbiKind::Uint(_)) => SqlType::Blob,
            VisitKind::Value(&AbiKind::Address) => SqlType::Blob,
            VisitKind::Value(&AbiKind::Bool) => SqlType::Integer,
            VisitKind::Value(&AbiKind::FixedBytes(_)) => SqlType::Blob,
            VisitKind::Value(&AbiKind::Function) => SqlType::Blob,
            VisitKind::Value(&AbiKind::Bytes) => SqlType::Blob,
            VisitKind::Value(&AbiKind::String) => SqlType::Blob,
            VisitKind::ArrayStart => {
                table_index = tables.len();
                tables.push(Table(vec![]));
                return;
            }
            VisitKind::ArrayEnd => {
                table_index = 0;
                return;
            }
            _ => unreachable!(),
        };
        tables[table_index].0.push(Column(type_));
    };
    event_visitor::visit_kind(value, &mut visitor);
}

#[cfg(test)]
mod tests {
    use solabi::{
        abi::{EventField, Field},
        ethprim::Address,
        function::{ExternalFunction, Selector},
        value::{Array, BitWidth, ByteLength, FixedBytes, Int, Uint},
    };

    use super::*;

    #[test]
    fn new_for_test() {
        Sqlite::new_for_test();
    }

    fn event_descriptor(values: Vec<AbiKind>) -> EventDescriptor {
        EventDescriptor {
            name: Default::default(),
            inputs: values
                .into_iter()
                .enumerate()
                .map(|(i, value)| EventField {
                    field: Field {
                        name: format!("field {i}"),
                        kind: value,
                        components: Default::default(),
                        internal_type: Default::default(),
                    },
                    indexed: Default::default(),
                })
                .collect(),
            anonymous: Default::default(),
        }
    }

    #[test]
    fn map_value_simple() {
        let values = vec![AbiKind::Bytes, AbiKind::Bool];
        let schema = event_to_tables(&event_descriptor(values)).unwrap();
        let expected = vec![Table(vec![Column(SqlType::Blob), Column(SqlType::Integer)])];
        assert_eq!(schema, expected);
    }

    #[test]
    fn map_value_complex_flat() {
        let values = vec![
            AbiKind::Bool,
            AbiKind::Tuple(vec![AbiKind::Bytes, AbiKind::Bool]),
            AbiKind::Bool,
            AbiKind::FixedArray(2, Box::new(AbiKind::Bytes)),
            AbiKind::Bool,
            AbiKind::Tuple(vec![AbiKind::Tuple(vec![AbiKind::FixedArray(
                2,
                Box::new(AbiKind::Bytes),
            )])]),
            AbiKind::Bool,
            AbiKind::FixedArray(
                2,
                Box::new(AbiKind::FixedArray(2, Box::new(AbiKind::Bytes))),
            ),
        ];
        let schema = event_to_tables(&event_descriptor(values)).unwrap();
        let expected = vec![Table(vec![
            Column(SqlType::Integer),
            // first tuple
            Column(SqlType::Blob),
            Column(SqlType::Integer),
            //
            Column(SqlType::Integer),
            // first fixed array
            Column(SqlType::Blob),
            Column(SqlType::Blob),
            //
            Column(SqlType::Integer),
            // second tuple
            Column(SqlType::Blob),
            Column(SqlType::Blob),
            //
            Column(SqlType::Integer),
            // second fixed array
            Column(SqlType::Blob),
            Column(SqlType::Blob),
            Column(SqlType::Blob),
            Column(SqlType::Blob),
        ])];
        assert_eq!(schema, expected);
    }

    #[test]
    fn map_value_array() {
        let values = vec![
            AbiKind::Bool,
            AbiKind::Array(Box::new(AbiKind::Bytes)),
            AbiKind::Bool,
            AbiKind::Array(Box::new(AbiKind::Bool)),
            AbiKind::Bool,
        ];
        let schema = event_to_tables(&event_descriptor(values)).unwrap();
        let expected = vec![
            Table(vec![
                Column(SqlType::Integer),
                Column(SqlType::Integer),
                Column(SqlType::Integer),
            ]),
            Table(vec![Column(SqlType::Blob)]),
            Table(vec![Column(SqlType::Integer)]),
        ];
        assert_eq!(schema, expected);
    }

    #[test]
    fn full_leaf_types() {
        let mut sqlite = Sqlite::new_for_test();
        let values = vec![
            AbiKind::Int(BitWidth::MIN),
            AbiKind::Uint(BitWidth::MIN),
            AbiKind::Address,
            AbiKind::Bool,
            AbiKind::FixedBytes(ByteLength::MIN),
            AbiKind::Function,
            AbiKind::Bytes,
            AbiKind::String,
        ];
        let event = event_descriptor(values);
        sqlite.prepare_event("event1", &event).unwrap();

        let fields = vec![
            AbiValue::Int(Int::new(8, 1i32.into()).unwrap()),
            AbiValue::Uint(Uint::new(8, 2u32.into()).unwrap()),
            AbiValue::Address(Address([3; 20])),
            AbiValue::Bool(true),
            AbiValue::FixedBytes(FixedBytes::new(&[4]).unwrap()),
            AbiValue::Function(ExternalFunction {
                address: Address([6; 20]),
                selector: Selector([7, 8, 9, 10]),
            }),
            AbiValue::Bytes(vec![11, 12]),
            AbiValue::String("abcd".to_string()),
        ];
        sqlite
            .update(
                &[],
                &[Log {
                    event: "event1",
                    block_number: 1,
                    log_index: 2,
                    transaction_index: 3,
                    address: Address([4; 20]),
                    fields,
                }],
            )
            .unwrap();

        let mut statement = sqlite.connection.prepare("SELECT * from event1_0").unwrap();
        let mut rows = statement.query(()).unwrap();
        while let Some(row) = rows.next().unwrap() {
            assert_eq!(row.as_ref().column_count(), FIXED_COLUMNS_COUNT + 8);
            for i in 0..row.as_ref().column_count() {
                let name = row.as_ref().column_name(i).unwrap();
                let value = row.get_ref(i).unwrap();
                println!("{name}: {value:?}");
            }
            println!();
        }
    }

    #[test]
    fn with_array() {
        let mut sqlite = Sqlite::new_for_test();
        let values = vec![AbiKind::Array(Box::new(AbiKind::Tuple(vec![
            AbiKind::Bool,
            AbiKind::String,
        ])))];
        let event = event_descriptor(values);
        sqlite.prepare_event("event1", &event).unwrap();

        let log = Log {
            event: "event1",
            block_number: 0,
            fields: vec![AbiValue::Array(
                Array::from_values(vec![
                    AbiValue::Tuple(vec![
                        AbiValue::Bool(false),
                        AbiValue::String("hello".to_string()),
                    ]),
                    AbiValue::Tuple(vec![
                        AbiValue::Bool(true),
                        AbiValue::String("world".to_string()),
                    ]),
                ])
                .unwrap(),
            )],
            ..Default::default()
        };
        sqlite.update(&[], &[log]).unwrap();

        let log = Log {
            event: "event1",
            block_number: 1,
            fields: vec![AbiValue::Array(
                Array::new(AbiKind::Tuple(vec![AbiKind::Bool, AbiKind::String]), vec![]).unwrap(),
            )],
            ..Default::default()
        };
        sqlite.update(&[], &[log]).unwrap();

        let mut statement = sqlite.connection.prepare("SELECT * from event1_0").unwrap();
        let mut rows = statement.query(()).unwrap();
        while let Some(row) = rows.next().unwrap() {
            assert_eq!(row.as_ref().column_count(), FIXED_COLUMNS_COUNT);
            for i in 0..row.as_ref().column_count() {
                let column = row.get_ref(i).unwrap();
                println!("{:?}", column);
            }
            println!();
        }

        let mut statement = sqlite.connection.prepare("SELECT * from event1_1").unwrap();
        let mut rows = statement.query(()).unwrap();
        while let Some(row) = rows.next().unwrap() {
            assert_eq!(row.as_ref().column_count(), FIXED_COLUMNS_COUNT + 3);
            for i in 0..row.as_ref().column_count() {
                let column = row.get_ref(i).unwrap();
                println!("{:?}", column);
            }
            println!();
        }
    }

    #[test]
    fn event_blocks() {
        let mut sqlite = Sqlite::new_for_test();
        sqlite
            .prepare_event("event", &event_descriptor(vec![]))
            .unwrap();
        let result = sqlite.event_block("event").unwrap();
        assert_eq!(result.indexed, 0);
        assert_eq!(result.finalized, 0);
        let blocks = database::EventBlock {
            event: "event",
            block: database::Block {
                indexed: 2,
                finalized: 3,
            },
        };
        sqlite.update(&[blocks], &[]).unwrap();
        let result = sqlite.event_block("event").unwrap();
        assert_eq!(result.indexed, 2);
        assert_eq!(result.finalized, 3);
    }

    #[test]
    fn remove() {
        let mut sqlite = Sqlite::new_for_test();
        sqlite
            .prepare_event("event", &event_descriptor(vec![]))
            .unwrap();
        sqlite
            .prepare_event("eventAAA", &event_descriptor(vec![]))
            .unwrap();
        sqlite
            .update(
                &[],
                &[
                    Log {
                        event: "event",
                        block_number: 1,
                        ..Default::default()
                    },
                    Log {
                        event: "event",
                        block_number: 2,
                        ..Default::default()
                    },
                    Log {
                        event: "event",
                        block_number: 5,
                        ..Default::default()
                    },
                    Log {
                        event: "event",
                        block_number: 6,
                        ..Default::default()
                    },
                ],
            )
            .unwrap();

        let rows = |sqlite: &Sqlite| {
            let count: i64 = sqlite
                .connection
                .query_row("SELECT COUNT(*) FROM event_0", (), |row| row.get(0))
                .unwrap();
            count
        };
        assert_eq!(rows(&sqlite), 4);

        sqlite
            .remove(&[database::Uncle {
                event: "event",
                number: 6,
            }])
            .unwrap();
        assert_eq!(rows(&sqlite), 3);

        sqlite
            .remove(&[database::Uncle {
                event: "eventAAA",
                number: 1,
            }])
            .unwrap();
        assert_eq!(rows(&sqlite), 3);

        sqlite
            .remove(&[database::Uncle {
                event: "event",
                number: 1,
            }])
            .unwrap();
        assert_eq!(rows(&sqlite), 0);
    }

    #[test]
    fn named_tuple() {
        let event = r#"
event OrderPlacement(
    address indexed sender,
    (
      address sellToken,
      address buyToken,
      address receiver,
      uint256 sellAmount,
      uint256 buyAmount,
      uint32 validTo,
      bytes32 appData,
      uint256 feeAmount,
      bytes32 kind,
      bool partiallyFillable,
      bytes32 sellTokenBalance,
      bytes32 buyTokenBalance
    ) order,
    (
      uint8 scheme,
      bytes data
    ) signature,
    bytes data
  )
"#;
        let event = EventDescriptor::parse_declaration(event).unwrap();
        let mut s = Sqlite::new_for_test();
        s.prepare_event("event", &event).unwrap();
        // TODO: Check that the column names are right.
    }
}