// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The sky's star catalogue, as tuile keeps it, and the table its shaders
//! read, made from it.
//!
//! Two steps, each a binary of this crate, each run once and its output
//! committed:
//!
//! 1. `convert`: the Bright Star Catalogue, 5th Revised Edition (Hoffleit
//!    & Warren 1991), as the CDS distributes it (catalogue V/50, fixed
//!    columns), into `data/bright_stars.parquet` — typed columns, each
//!    described in its field metadata, the source's URL and SHA-256 in the
//!    file's.
//! 2. `table`: that Parquet file into `tuile-atmosphere/src/stars.table`,
//!    the binned texture `stars_seen` reads ([`tuile_atmosphere::stars::StarTable`]).
//!
//! ```text
//! mkdir /tmp/bsc5 && cd /tmp/bsc5 && curl -O https://cdsarc.cds.unistra.fr/ftp/V/50/catalog.gz
//! cargo run -p tuile-star-catalogue --bin convert -- /tmp/bsc5/catalog.gz \
//!     crates/tuile-star-catalogue/data/bright_stars.parquet
//! cargo run -p tuile-star-catalogue --bin table -- \
//!     crates/tuile-star-catalogue/data/bright_stars.parquet crates/tuile-atmosphere/src/stars.table
//! ```
//!
//! Parquet and Arrow live here and nowhere else: no renderer links this
//! crate, and nothing of it reaches a wasm build.

use std::path::Path;
use std::sync::Arc;

use arrow_array::{Array, Float64Array, RecordBatch, UInt32Array};
use arrow_schema::{DataType, Field, Schema};
use glam::DVec3;
use tuile_atmosphere::stars::Star;

/// Where the source is published.
pub const SOURCE_URL: &str = "https://cdsarc.cds.unistra.fr/ftp/V/50/catalog.gz";

/// The SHA-256 of the source file as it was converted (`catalog.gz`, the
/// CDS's V/50 as fetched on 2026-10-25).
pub const SOURCE_SHA256: &str = "3dc44b1e90be8fbe5bcc7656032560f51275f985c7e3f783c9028e1838ec7bed";

/// The SHA-256 of the uncompressed `catalog` inside it.
pub const SOURCE_UNCOMPRESSED_SHA256: &str =
    "69797549cc1605aad7ff94e9325e29a1661f2a253917faaa056d9bf20b809afd";

/// One row of the catalogue.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Row {
    /// The Bright Star Catalogue's number (HR).
    pub hr: u32,
    /// Right ascension and declination, equinox and epoch J2000, degrees.
    pub ra_deg: f64,
    pub dec_deg: f64,
    /// Proper motion, J2000, FK5, arcseconds a year: in right ascension
    /// projected on the sky (cos δ · dα/dt), and in declination.
    pub pm_ra_cosdec: Option<f64>,
    pub pm_dec: Option<f64>,
    /// Visual magnitude, and the B−V colour index.
    pub v: f64,
    pub b_v: Option<f64>,
}

impl Row {
    /// The star the sky draws.
    pub fn star(&self) -> Star {
        let (ra, dec) = (self.ra_deg.to_radians(), self.dec_deg.to_radians());
        Star {
            hr: self.hr,
            direction: DVec3::new(dec.cos() * ra.cos(), dec.cos() * ra.sin(), dec.sin()),
            v: self.v,
            b_v: self.b_v,
        }
    }
}

fn field(line: &str, from: usize, to: usize) -> &str {
    // The ReadMe counts bytes from 1, both ends included.
    line.get(from - 1..to.min(line.len())).unwrap_or("").trim()
}

/// The rows of the CDS's fixed-column `catalog`: every entry with a J2000
/// position and a visual magnitude (9 096 of 9 110; the others are novae,
/// clusters and withdrawn entries). Bytes as its `ReadMe` lays them out.
pub fn read_cds(text: &str) -> Result<Vec<Row>, String> {
    let mut rows = Vec::new();
    for line in text.lines() {
        let Ok(hr) = field(line, 1, 4).parse::<u32>() else {
            continue;
        };
        if field(line, 76, 77).is_empty() || field(line, 103, 107).is_empty() {
            continue;
        }
        let number = |from, to| -> Result<f64, String> {
            field(line, from, to)
                .parse()
                .map_err(|_| format!("HR {hr}: bytes {from}-{to}"))
        };
        let optional = |from, to| -> Result<Option<f64>, String> {
            match field(line, from, to) {
                "" => Ok(None),
                s => s
                    .parse()
                    .map(Some)
                    .map_err(|_| format!("HR {hr}: bytes {from}-{to}")),
            }
        };
        let ra = (number(76, 77)? + number(78, 79)? / 60.0 + number(80, 83)? / 3600.0) * 15.0;
        let mut dec = number(85, 86)? + number(87, 88)? / 60.0 + number(89, 90)? / 3600.0;
        if field(line, 84, 84) == "-" {
            dec = -dec;
        }
        rows.push(Row {
            hr,
            ra_deg: ra,
            dec_deg: dec,
            pm_ra_cosdec: optional(149, 154)?,
            pm_dec: optional(155, 160)?,
            v: number(103, 107)?,
            b_v: optional(110, 114)?,
        });
    }
    Ok(rows)
}

fn described(name: &str, ty: DataType, nullable: bool, what: &str) -> Field {
    Field::new(name, ty, nullable).with_metadata(
        [("description".to_owned(), what.to_owned())]
            .into_iter()
            .collect(),
    )
}

/// The Parquet file's schema, every column described.
pub fn schema() -> Schema {
    Schema::new(vec![
        described("hr", DataType::UInt32, false, "Bright Star Catalogue (Harvard Revised) number"),
        described("ra_j2000_deg", DataType::Float64, false, "Right ascension, equinox and epoch J2000, degrees"),
        described("dec_j2000_deg", DataType::Float64, false, "Declination, equinox and epoch J2000, degrees"),
        described("pm_ra_cosdec_arcsec_per_year", DataType::Float64, true, "Proper motion in right ascension, cos(dec) d(ra)/dt, J2000 FK5, arcseconds per year"),
        described("pm_dec_arcsec_per_year", DataType::Float64, true, "Proper motion in declination, J2000 FK5, arcseconds per year"),
        described("v_mag", DataType::Float64, false, "Visual (Johnson V) magnitude"),
        described("b_v_mag", DataType::Float64, true, "B-V colour index, UBV system"),
    ])
    .with_metadata(
        [
            ("source".to_owned(), "Bright Star Catalogue, 5th Revised Ed. (Hoffleit & Warren 1991), CDS catalogue V/50".to_owned()),
            ("source_url".to_owned(), SOURCE_URL.to_owned()),
            ("source_sha256".to_owned(), SOURCE_SHA256.to_owned()),
            ("source_uncompressed_sha256".to_owned(), SOURCE_UNCOMPRESSED_SHA256.to_owned()),
        ]
        .into_iter()
        .collect(),
    )
}

/// `rows` as a Parquet file at `path`.
pub fn write_parquet(rows: &[Row], path: &Path) -> Result<(), String> {
    let schema = Arc::new(schema());
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt32Array::from_iter_values(rows.iter().map(|r| r.hr))),
            Arc::new(Float64Array::from_iter_values(
                rows.iter().map(|r| r.ra_deg),
            )),
            Arc::new(Float64Array::from_iter_values(
                rows.iter().map(|r| r.dec_deg),
            )),
            Arc::new(Float64Array::from_iter(rows.iter().map(|r| r.pm_ra_cosdec))),
            Arc::new(Float64Array::from_iter(rows.iter().map(|r| r.pm_dec))),
            Arc::new(Float64Array::from_iter_values(rows.iter().map(|r| r.v))),
            Arc::new(Float64Array::from_iter(rows.iter().map(|r| r.b_v))),
        ],
    )
    .map_err(|e| e.to_string())?;
    let file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let props = parquet::file::properties::WriterProperties::builder()
        .set_compression(parquet::basic::Compression::SNAPPY)
        .build();
    let mut writer = parquet::arrow::ArrowWriter::try_new(file, schema, Some(props))
        .map_err(|e| e.to_string())?;
    writer.write(&batch).map_err(|e| e.to_string())?;
    writer.close().map_err(|e| e.to_string())?;
    Ok(())
}

/// The rows of a Parquet file [`write_parquet`] wrote.
pub fn read_parquet(path: &Path) -> Result<Vec<Row>, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| e.to_string())?
        .build()
        .map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    for batch in reader {
        let batch = batch.map_err(|e| e.to_string())?;
        let column = |name: &str| {
            batch
                .column_by_name(name)
                .ok_or(format!("no column {name}"))
        };
        let f64s = |name: &str| -> Result<Float64Array, String> {
            column(name)?
                .as_any()
                .downcast_ref::<Float64Array>()
                .cloned()
                .ok_or(format!("{name}: not f64"))
        };
        let hr = column("hr")?
            .as_any()
            .downcast_ref::<UInt32Array>()
            .cloned()
            .ok_or("hr: not u32")?;
        let (ra, dec, pm_ra, pm_dec, v, b_v) = (
            f64s("ra_j2000_deg")?,
            f64s("dec_j2000_deg")?,
            f64s("pm_ra_cosdec_arcsec_per_year")?,
            f64s("pm_dec_arcsec_per_year")?,
            f64s("v_mag")?,
            f64s("b_v_mag")?,
        );
        let optional = |a: &Float64Array, i: usize| (!a.is_null(i)).then(|| a.value(i));
        for i in 0..batch.num_rows() {
            rows.push(Row {
                hr: hr.value(i),
                ra_deg: ra.value(i),
                dec_deg: dec.value(i),
                pm_ra_cosdec: optional(&pm_ra, i),
                pm_dec: optional(&pm_dec, i),
                v: v.value(i),
                b_v: optional(&b_v, i),
            });
        }
    }
    Ok(rows)
}

/// The CDS's `catalog.gz`, checked against [`SOURCE_SHA256`] and
/// uncompressed: its rows.
pub fn read_cds_gz(path: &Path) -> Result<Vec<Row>, String> {
    use sha2::Digest;
    use std::io::Read;
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let sha: String = sha2::Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if sha != SOURCE_SHA256 {
        return Err(format!(
            "{}: SHA-256 {sha}, not the {SOURCE_SHA256} converted — a new release of the source; \
             check it and record its checksum",
            path.display()
        ));
    }
    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(&bytes[..])
        .read_to_end(&mut raw)
        .map_err(|e| e.to_string())?;
    // ASCII; Latin-1 keeps a stray byte one character wide.
    read_cds(&raw.iter().map(|&b| char::from(b)).collect::<String>())
}

/// The committed catalogue.
pub fn committed() -> Vec<Row> {
    read_parquet(&Path::new(env!("CARGO_MANIFEST_DIR")).join("data/bright_stars.parquet"))
        .expect("data/bright_stars.parquet")
}
