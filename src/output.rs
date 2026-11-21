use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow_array::builder::StringBuilder;
use arrow_array::{ArrayRef, Date32Array, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use parquet::schema::types::ColumnPath;

use crate::aggregate::{CampaignDays, UserDay};

pub type Error = Box<dyn std::error::Error + Send + Sync>;
struct Table {
    writer: ArrowWriter<File>,
    schema: SchemaRef,
}

impl Table {
    fn new(path: &Path, fields: Vec<Field>) -> Result<Self, Error> {
        let mut properties = WriterProperties::builder().set_compression(Compression::SNAPPY);
        if fields[0].name() == "user_id" {
            properties =
                properties.set_column_dictionary_enabled(ColumnPath::from("user_id"), false);
        }
        let properties = properties.build();
        let schema = Arc::new(Schema::new(fields));
        let file = File::create(path)?;
        let writer = ArrowWriter::try_new(file, schema.clone(), Some(properties))?;
        Ok(Self { writer, schema })
    }

    fn write(&mut self, columns: Vec<ArrayRef>) -> Result<(), Error> {
        self.writer
            .write(&RecordBatch::try_new(self.schema.clone(), columns)?)?;
        Ok(())
    }

    fn finish(self) -> Result<(), Error> {
        self.writer.close()?;
        Ok(())
    }
}

pub struct Users(Table);

impl Users {
    pub fn new(path: &Path) -> Result<Self, Error> {
        use DataType::{Date32, Int64, Utf8};
        Ok(Self(Table::new(
            path,
            vec![
                Field::new("user_id", Utf8, false),
                Field::new("day", Date32, false),
                Field::new("sessions", Int64, false),
                Field::new("events", Int64, false),
                Field::new("revenue_cents", Int64, false),
                Field::new("top_category", Utf8, true),
            ],
        )?))
    }

    pub fn write(&mut self, rows: &[UserDay<'_>], interned: &[&[u8]]) -> Result<(), Error> {
        let mut people = StringBuilder::with_capacity(rows.len(), rows.len() * 9);
        let mut buffer = [0; 9];
        for group in rows.chunk_by(|a, b| a.person == b.person) {
            let name = group[0].person.name(interned, &mut buffer)?;
            for _ in group {
                people.append_value(name);
            }
        }
        let columns: Vec<ArrayRef> = vec![
            Arc::new(people.finish()),
            Arc::new(Date32Array::from_iter_values(rows.iter().map(|r| r.day))),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|r| r.sessions),
            )),
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.events))),
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.revenue))),
            Arc::new(StringArray::from_iter(rows.iter().map(|r| r.top))),
        ];
        self.0.write(columns)
    }

    pub fn finish(self) -> Result<(), Error> {
        self.0.finish()
    }
}

pub fn campaigns(path: &Path, rows: CampaignDays, labels: &[(&str, &str)]) -> Result<(), Error> {
    use DataType::{Date32, Int64, Utf8};
    let mut table = Table::new(
        path,
        vec![
            Field::new("campaign_id", Utf8, false),
            Field::new("channel", Utf8, false),
            Field::new("day", Date32, false),
            Field::new("sessions", Int64, false),
            Field::new("purchasing_sessions", Int64, false),
            Field::new("revenue_cents", Int64, false),
            Field::new("assisted_sessions", Int64, false),
        ],
    )?;
    let rows = rows.into_rows();
    let columns: Vec<ArrayRef> = vec![
        Arc::new(StringArray::from_iter_values(
            rows.iter().map(|((c, _), _)| labels[*c as usize].0),
        )),
        Arc::new(StringArray::from_iter_values(
            rows.iter().map(|((c, _), _)| labels[*c as usize].1),
        )),
        Arc::new(Date32Array::from_iter_values(
            rows.iter().map(|((_, day), _)| *day),
        )),
        Arc::new(Int64Array::from_iter_values(
            rows.iter().map(|(_, r)| r.sessions),
        )),
        Arc::new(Int64Array::from_iter_values(
            rows.iter().map(|(_, r)| r.purchasing_sessions),
        )),
        Arc::new(Int64Array::from_iter_values(
            rows.iter().map(|(_, r)| r.revenue),
        )),
        Arc::new(Int64Array::from_iter_values(
            rows.iter().map(|(_, r)| r.assists),
        )),
    ];
    table.write(columns)?;
    table.finish()
}
