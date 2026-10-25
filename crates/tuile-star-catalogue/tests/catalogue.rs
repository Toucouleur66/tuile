// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The committed catalogue is the Bright Star Catalogue, and the committed
//! table is that catalogue binned: nothing drifts between the two files.

use glam::DVec3;
use tuile_atmosphere::stars::{StarTable, STAR_REACH};

#[test]
fn the_parquet_catalogue_is_the_bright_star_catalogue() {
    let rows = tuile_star_catalogue::committed();
    assert_eq!(rows.len(), 9_096);
    let sirius = rows.iter().find(|r| r.hr == 2491).expect("Sirius");
    assert_eq!(sirius.v, -1.46);
    assert_eq!(sirius.b_v, Some(0.0));
    assert!(
        (sirius.ra_deg - 101.287_083).abs() < 1e-5,
        "{}",
        sirius.ra_deg
    );
    assert!(
        (sirius.dec_deg + 16.716_111).abs() < 1e-5,
        "{}",
        sirius.dec_deg
    );
    // Its proper motion, −0.553″ and −1.205″ a year.
    assert_eq!(sirius.pm_ra_cosdec, Some(-0.553));
    assert_eq!(sirius.pm_dec, Some(-1.205));
    // Every column says what it holds; the file says where it came from.
    let schema = tuile_star_catalogue::schema();
    assert!(schema
        .fields()
        .iter()
        .all(|f| f.metadata().contains_key("description")));
    assert_eq!(
        schema.metadata()["source_sha256"],
        tuile_star_catalogue::SOURCE_SHA256
    );
}

#[test]
fn the_committed_table_is_the_catalogue_binned() {
    let stars: Vec<_> = tuile_star_catalogue::committed()
        .iter()
        .map(|r| r.star())
        .collect();
    let made = StarTable::of(&stars).to_bytes();
    let committed = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tuile-atmosphere/src/stars.table"),
    )
    .expect("stars.table");
    assert!(
        made == committed,
        "stars.table is not the catalogue's: run the `table` binary again"
    );
}

#[test]
fn every_star_whose_image_reaches_a_line_of_sight_is_in_its_cell() {
    let table = StarTable::catalogue();
    let stars: Vec<_> = tuile_star_catalogue::committed()
        .iter()
        .map(|r| {
            let s = r.star();
            (s.direction, s.irradiance(), s.colour())
        })
        .collect();
    let sigma = 0.25 * STAR_REACH;
    let k = 1.0 / (2.0 * sigma * sigma);
    // Some hundred lines of sight near stars, at offsets up to the reach:
    // what the table finds is every star within the reach.
    for (n, &(s, _, _)) in stars.iter().step_by(90).enumerate() {
        let a = s.any_orthonormal_vector();
        let off = 0.9 * STAR_REACH * (n as f64 * 0.37).fract();
        let phi = n as f64 * 2.4;
        let c: DVec3 = (s + (a * phi.cos() + s.cross(a) * phi.sin()) * off).normalize();
        let got = table.radiance(c, sigma);
        let mut want = [0.0; 3];
        for &(t, e, colour) in &stars {
            if (c - t).length() > STAR_REACH {
                continue;
            }
            let w = e * (-(c - t).length_squared() * k).exp() * k / std::f64::consts::PI;
            for ch in 0..3 {
                want[ch] += w * colour[ch];
            }
        }
        for ch in 0..3 {
            assert!(
                (got[ch] - want[ch]).abs() <= 1e-3 * want[ch] + 1e-30,
                "line {n}: {got:?} against {want:?}"
            );
        }
    }
}

/// With `TUILE_BSC5_GZ` set to the CDS's `catalog.gz`, the conversion
/// gives the committed rows again.
#[test]
fn converting_the_source_again_gives_the_committed_rows() {
    let Ok(path) = std::env::var("TUILE_BSC5_GZ") else {
        eprintln!("skipping: TUILE_BSC5_GZ is not set");
        return;
    };
    let rows = tuile_star_catalogue::read_cds_gz(path.as_ref()).expect("the source");
    assert_eq!(rows, tuile_star_catalogue::committed());
}
