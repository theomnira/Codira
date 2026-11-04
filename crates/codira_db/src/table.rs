//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - Typed tables over the key-value engine: a [`Schema`] (name, columns of
//!   scalar types, a single-column primary key, a version number) stored in
//!   a catalog key, and rows encoded into keys ordered by primary key.
//! - An explicit migration hook, [`Table::open_migrated`], that upgrades a
//!   stored schema version by version. The only migration step is
//!   [`Migration::AddColumn`] with a default.
//!
//! Key layout (keys starting with byte `0xFF` are reserved for this layer):
//! - catalog: `0xFF "codira_db/catalog/" name` -> `version u32 | schema`
//! - rows: `0xFF "codira_db/rows/" len(name) u16 BE | name | pk` ->
//!   `row version u32 | column count u16 | values`
//!
//! The primary key is encoded so that byte order equals value order:
//! `I64` as big-endian with the sign bit flipped, `Bool` as one byte,
//! `Text` and `Bytes` as their raw bytes (the key ends there, so no
//! terminator is needed). `F64` cannot be a primary key.
//!
//! Migrations are lazy: adding a column rewrites only the catalog. Each row
//! records the schema version it was written with; reading a row written
//! before a column existed yields that column's default. This is also what
//! keeps a concurrent writer that still uses the old schema correct: its
//! rows carry the old version and are upgraded on read.

use std::ops::Bound;

use crate::{
    codec::{Reader, Writer},
    db::prefix_end,
    Error, Result, Txn,
};

/// The scalar column types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnType {
    /// 64-bit signed integer.
    I64,
    /// 64-bit IEEE float (not allowed as a primary key).
    F64,
    /// Boolean.
    Bool,
    /// UTF-8 text.
    Text,
    /// Raw bytes.
    Bytes,
}

impl ColumnType {
    fn tag(self) -> u8 {
        match self {
            ColumnType::I64 => 1,
            ColumnType::F64 => 2,
            ColumnType::Bool => 3,
            ColumnType::Text => 4,
            ColumnType::Bytes => 5,
        }
    }
    fn from_tag(tag: u8) -> Option<ColumnType> {
        Some(match tag {
            1 => ColumnType::I64,
            2 => ColumnType::F64,
            3 => ColumnType::Bool,
            4 => ColumnType::Text,
            5 => ColumnType::Bytes,
            _ => return None,
        })
    }
}

/// A column value. There is no NULL: every column of every row has a value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// An `I64` value.
    I64(i64),
    /// An `F64` value.
    F64(f64),
    /// A `Bool` value.
    Bool(bool),
    /// A `Text` value.
    Text(String),
    /// A `Bytes` value.
    Bytes(Vec<u8>),
}

impl Value {
    /// The type of this value.
    pub fn ty(&self) -> ColumnType {
        match self {
            Value::I64(_) => ColumnType::I64,
            Value::F64(_) => ColumnType::F64,
            Value::Bool(_) => ColumnType::Bool,
            Value::Text(_) => ColumnType::Text,
            Value::Bytes(_) => ColumnType::Bytes,
        }
    }

    fn encode(&self, w: &mut Writer) {
        w.u8(self.ty().tag());
        match self {
            Value::I64(v) => w.u64(*v as u64),
            Value::F64(v) => w.u64(v.to_bits()),
            Value::Bool(v) => w.u8(u8::from(*v)),
            Value::Text(v) => w.bytes(v.as_bytes()),
            Value::Bytes(v) => w.bytes(v),
        }
    }

    fn decode(r: &mut Reader<'_>) -> Option<Value> {
        Some(match ColumnType::from_tag(r.u8()?)? {
            ColumnType::I64 => Value::I64(r.u64()? as i64),
            ColumnType::F64 => Value::F64(f64::from_bits(r.u64()?)),
            ColumnType::Bool => match r.u8()? {
                0 => Value::Bool(false),
                1 => Value::Bool(true),
                _ => return None,
            },
            ColumnType::Text => Value::Text(String::from_utf8(r.bytes()?.to_vec()).ok()?),
            ColumnType::Bytes => Value::Bytes(r.bytes()?.to_vec()),
        })
    }

    /// The order-preserving primary-key encoding.
    fn key_bytes(&self) -> Result<Vec<u8>> {
        Ok(match self {
            Value::I64(v) => ((*v as u64) ^ (1 << 63)).to_be_bytes().to_vec(),
            Value::Bool(v) => vec![u8::from(*v)],
            Value::Text(v) => v.as_bytes().to_vec(),
            Value::Bytes(v) => v.clone(),
            Value::F64(_) => return Err(Error::Schema("F64 cannot be a primary key".into())),
        })
    }
}

/// A column definition.
#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    /// The column name, unique within its table.
    pub name: String,
    /// The column type.
    pub ty: ColumnType,
    /// The value rows written before this column existed read as. Required
    /// for a column added by a migration.
    pub default: Option<Value>,
    added_in: u32,
}

impl Column {
    /// A column with no default.
    pub fn new(name: impl Into<String>, ty: ColumnType) -> Column {
        Column {
            name: name.into(),
            ty,
            default: None,
            added_in: 1,
        }
    }

    /// A column with a default, as a migration's added column needs.
    pub fn with_default(name: impl Into<String>, ty: ColumnType, default: Value) -> Column {
        Column {
            default: Some(default),
            ..Column::new(name, ty)
        }
    }

    /// The schema version that added this column (1 for original columns).
    pub fn added_in(&self) -> u32 {
        self.added_in
    }
}

/// A table schema.
#[derive(Clone, Debug, PartialEq)]
pub struct Schema {
    name: String,
    version: u32,
    columns: Vec<Column>,
    primary_key: usize,
}

fn check_name(what: &str, name: &str) -> Result<()> {
    if name.is_empty() || name.len() > u16::MAX as usize {
        return Err(Error::Schema(format!(
            "{what} name must be 1 to 65535 bytes"
        )));
    }
    Ok(())
}

fn check_default(c: &Column) -> Result<()> {
    if let Some(d) = &c.default {
        if d.ty() != c.ty {
            return Err(Error::Schema(format!(
                "default of column `{}` is {:?}, column is {:?}",
                c.name,
                d.ty(),
                c.ty
            )));
        }
    }
    Ok(())
}

impl Schema {
    /// A version-1 schema. `primary_key` names one of the columns.
    pub fn new(name: impl Into<String>, columns: Vec<Column>, primary_key: &str) -> Result<Schema> {
        let name = name.into();
        check_name("table", &name)?;
        if columns.is_empty() || columns.len() > u16::MAX as usize {
            return Err(Error::Schema("a table needs 1 to 65535 columns".into()));
        }
        let mut columns = columns;
        for c in columns.iter_mut() {
            check_name("column", &c.name)?;
            check_default(c)?;
            c.added_in = 1;
        }
        for (i, c) in columns.iter().enumerate() {
            if columns[..i].iter().any(|o| o.name == c.name) {
                return Err(Error::Schema(format!("duplicate column `{}`", c.name)));
            }
        }
        let pk = columns
            .iter()
            .position(|c| c.name == primary_key)
            .ok_or_else(|| Error::Schema(format!("no primary-key column `{primary_key}`")))?;
        if columns[pk].ty == ColumnType::F64 {
            return Err(Error::Schema("F64 cannot be a primary key".into()));
        }
        Ok(Schema {
            name,
            version: 1,
            columns,
            primary_key: pk,
        })
    }

    /// The table name.
    pub fn name(&self) -> &str {
        &self.name
    }
    /// The schema version: 1 at creation, plus one per migration.
    pub fn version(&self) -> u32 {
        self.version
    }
    /// The columns, in row order.
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }
    /// The index of the primary-key column.
    pub fn primary_key(&self) -> usize {
        self.primary_key
    }
    /// The index of the column called `name`.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u32(self.version);
        w.bytes(self.name.as_bytes());
        w.u16(self.primary_key as u16);
        w.u16(self.columns.len() as u16);
        for c in &self.columns {
            w.bytes(c.name.as_bytes());
            w.u8(c.ty.tag());
            w.u32(c.added_in);
            match &c.default {
                Some(d) => {
                    w.u8(1);
                    d.encode(&mut w);
                }
                None => w.u8(0),
            }
        }
        w.buf
    }

    fn decode(data: &[u8]) -> Result<Schema> {
        let bad = || Error::Schema("malformed catalog entry".into());
        let text = |b: &[u8]| String::from_utf8(b.to_vec()).ok();
        let mut r = Reader::new(data);
        let version = r.u32().ok_or_else(bad)?;
        let name = text(r.bytes().ok_or_else(bad)?).ok_or_else(bad)?;
        let primary_key = r.u16().ok_or_else(bad)? as usize;
        let n = r.u16().ok_or_else(bad)?;
        let mut columns = Vec::new();
        for _ in 0..n {
            let cname = text(r.bytes().ok_or_else(bad)?).ok_or_else(bad)?;
            let ty = ColumnType::from_tag(r.u8().ok_or_else(bad)?).ok_or_else(bad)?;
            let added_in = r.u32().ok_or_else(bad)?;
            let default = match r.u8().ok_or_else(bad)? {
                0 => None,
                1 => Some(Value::decode(&mut r).ok_or_else(bad)?),
                _ => return Err(bad()),
            };
            columns.push(Column {
                name: cname,
                ty,
                default,
                added_in,
            });
        }
        if !r.is_empty() || primary_key >= columns.len() {
            return Err(bad());
        }
        Ok(Schema {
            name,
            version,
            columns,
            primary_key,
        })
    }
}

/// One schema upgrade step.
#[derive(Clone, Debug, PartialEq)]
pub enum Migration {
    /// Appends a column. It must have a default, which existing rows read
    /// as. Rows are not rewritten.
    AddColumn(Column),
}

const CATALOG_PREFIX: &[u8] = b"\xFFcodira_db/catalog/";
const ROW_PREFIX: &[u8] = b"\xFFcodira_db/rows/";

fn catalog_key(table: &str) -> Vec<u8> {
    let mut k = CATALOG_PREFIX.to_vec();
    k.extend_from_slice(table.as_bytes());
    k
}

fn row_prefix(table: &str) -> Vec<u8> {
    let mut k = ROW_PREFIX.to_vec();
    k.extend_from_slice(&(table.len() as u16).to_be_bytes());
    k.extend_from_slice(table.as_bytes());
    k
}

/// A handle on a table, valid for transactions that see the same schema
/// version it was opened with (otherwise operations return
/// [`Error::SchemaChanged`]).
#[derive(Clone, Debug)]
pub struct Table {
    schema: Schema,
    prefix: Vec<u8>,
}

impl Table {
    fn from_schema(schema: Schema) -> Table {
        Table {
            prefix: row_prefix(&schema.name),
            schema,
        }
    }

    /// Creates a table in `txn`. Fails if a table with this name exists in
    /// the transaction's view. Two transactions creating the same table
    /// conflict.
    pub fn create(txn: &mut Txn<'_>, schema: Schema) -> Result<Table> {
        let key = catalog_key(&schema.name);
        if txn.get(&key).is_some() {
            return Err(Error::Schema(format!(
                "table `{}` already exists",
                schema.name
            )));
        }
        txn.put(&key, &schema.encode())?;
        Ok(Table::from_schema(schema))
    }

    /// Opens an existing table at its stored schema version.
    pub fn open(txn: &Txn<'_>, name: &str) -> Result<Table> {
        let data = txn
            .get(&catalog_key(name))
            .ok_or_else(|| Error::Schema(format!("no table `{name}`")))?;
        Ok(Table::from_schema(Schema::decode(&data)?))
    }

    /// The migration hook. Opens table `name` and brings its schema to
    /// version `migrations.len() + 1`: `migrations[i]` upgrades version
    /// `i + 1` to `i + 2`, and only the steps the stored version has not
    /// had yet are applied. Applying is idempotent across runs. Fails if the
    /// stored version is newer than the program knows. Two transactions
    /// migrating the same table conflict on its catalog key.
    pub fn open_migrated(txn: &mut Txn<'_>, name: &str, migrations: &[Migration]) -> Result<Table> {
        let mut schema = Table::open(txn, name)?.schema;
        let target = migrations.len() as u32 + 1;
        if schema.version > target {
            return Err(Error::Schema(format!(
                "table `{name}` is at schema version {}, newer than this program's {target}",
                schema.version
            )));
        }
        if schema.version == target {
            return Ok(Table::from_schema(schema));
        }
        for m in &migrations[schema.version as usize - 1..] {
            let next = schema.version + 1;
            match m {
                Migration::AddColumn(c) => {
                    check_name("column", &c.name)?;
                    if c.default.is_none() {
                        return Err(Error::Schema(format!(
                            "added column `{}` needs a default",
                            c.name
                        )));
                    }
                    check_default(c)?;
                    if schema.column_index(&c.name).is_some() {
                        return Err(Error::Schema(format!("duplicate column `{}`", c.name)));
                    }
                    if schema.columns.len() >= u16::MAX as usize {
                        return Err(Error::Schema("too many columns".into()));
                    }
                    let mut c = c.clone();
                    c.added_in = next;
                    schema.columns.push(c);
                }
            }
            schema.version = next;
        }
        txn.put(&catalog_key(name), &schema.encode())?;
        Ok(Table::from_schema(schema))
    }

    /// The schema this handle was opened with.
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    fn check_current(&self, txn: &Txn<'_>) -> Result<()> {
        let stale = || Error::SchemaChanged {
            table: self.schema.name.clone(),
        };
        let data = txn.get(&catalog_key(&self.schema.name)).ok_or_else(stale)?;
        if data.get(..4) != Some(&self.schema.version.to_le_bytes()[..]) {
            return Err(stale());
        }
        Ok(())
    }

    fn row_key(&self, pk: &Value) -> Result<Vec<u8>> {
        let ty = self.schema.columns[self.schema.primary_key].ty;
        if pk.ty() != ty {
            return Err(Error::Schema(format!(
                "primary key of `{}` is {ty:?}, got {:?}",
                self.schema.name,
                pk.ty()
            )));
        }
        let mut k = self.prefix.clone();
        k.extend(pk.key_bytes()?);
        Ok(k)
    }

    fn encode_row(&self, row: &[Value]) -> Result<(Vec<u8>, Vec<u8>)> {
        if row.len() != self.schema.columns.len() {
            return Err(Error::Schema(format!(
                "table `{}` has {} columns, row has {}",
                self.schema.name,
                self.schema.columns.len(),
                row.len()
            )));
        }
        let mut w = Writer::default();
        w.u32(self.schema.version);
        w.u16(row.len() as u16);
        for (c, v) in self.schema.columns.iter().zip(row) {
            if v.ty() != c.ty {
                return Err(Error::Schema(format!(
                    "column `{}` is {:?}, got {:?}",
                    c.name,
                    c.ty,
                    v.ty()
                )));
            }
            v.encode(&mut w);
        }
        crate::codec::check_len("row", w.buf.len())?;
        Ok((self.row_key(&row[self.schema.primary_key])?, w.buf))
    }

    fn decode_row(&self, data: &[u8]) -> Result<Vec<Value>> {
        let bad = || Error::Schema(format!("malformed row in table `{}`", self.schema.name));
        let mut r = Reader::new(data);
        let version = r.u32().ok_or_else(bad)?;
        if version > self.schema.version {
            return Err(Error::SchemaChanged {
                table: self.schema.name.clone(),
            });
        }
        let n = r.u16().ok_or_else(bad)? as usize;
        let stored = self
            .schema
            .columns
            .iter()
            .take_while(|c| c.added_in <= version)
            .count();
        if n != stored {
            return Err(bad());
        }
        let mut row = Vec::with_capacity(self.schema.columns.len());
        for c in &self.schema.columns[..stored] {
            let v = Value::decode(&mut r).ok_or_else(bad)?;
            if v.ty() != c.ty {
                return Err(bad());
            }
            row.push(v);
        }
        if !r.is_empty() {
            return Err(bad());
        }
        for c in &self.schema.columns[stored..] {
            row.push(c.default.clone().ok_or_else(bad)?);
        }
        Ok(row)
    }

    /// Inserts a row; fails with [`Error::DuplicateKey`] if its primary key
    /// is taken in the transaction's view.
    pub fn insert(&self, txn: &mut Txn<'_>, row: &[Value]) -> Result<()> {
        self.check_current(txn)?;
        let (key, value) = self.encode_row(row)?;
        if txn.get(&key).is_some() {
            return Err(Error::DuplicateKey);
        }
        txn.put(&key, &value)
    }

    /// Inserts or replaces a row.
    pub fn upsert(&self, txn: &mut Txn<'_>, row: &[Value]) -> Result<()> {
        self.check_current(txn)?;
        let (key, value) = self.encode_row(row)?;
        txn.put(&key, &value)
    }

    /// The row with primary key `pk`.
    pub fn get(&self, txn: &Txn<'_>, pk: &Value) -> Result<Option<Vec<Value>>> {
        self.check_current(txn)?;
        match txn.get(&self.row_key(pk)?) {
            Some(data) => Ok(Some(self.decode_row(&data)?)),
            None => Ok(None),
        }
    }

    /// Deletes the row with primary key `pk`; returns whether it existed.
    pub fn delete(&self, txn: &mut Txn<'_>, pk: &Value) -> Result<bool> {
        self.check_current(txn)?;
        let key = self.row_key(pk)?;
        let existed = txn.get(&key).is_some();
        if existed {
            txn.delete(&key)?;
        }
        Ok(existed)
    }

    /// The rows whose primary key is within the bounds, in key order.
    pub fn scan(
        &self,
        txn: &Txn<'_>,
        start: Bound<&Value>,
        end: Bound<&Value>,
    ) -> Result<Vec<Vec<Value>>> {
        self.check_current(txn)?;
        let map = |b: Bound<&Value>| -> Result<Bound<Vec<u8>>> {
            Ok(match b {
                Bound::Included(v) => Bound::Included(self.row_key(v)?),
                Bound::Excluded(v) => Bound::Excluded(self.row_key(v)?),
                Bound::Unbounded => Bound::Unbounded,
            })
        };
        let start = match map(start)? {
            Bound::Unbounded => Bound::Included(self.prefix.clone()),
            b => b,
        };
        let end = match map(end)? {
            Bound::Unbounded => match prefix_end(&self.prefix) {
                Some(e) => Bound::Excluded(e),
                None => Bound::Unbounded,
            },
            b => b,
        };
        txn.scan(
            start.as_ref().map(Vec::as_slice),
            end.as_ref().map(Vec::as_slice),
        )
        .into_iter()
        .map(|(_, v)| self.decode_row(&v))
        .collect()
    }
}
