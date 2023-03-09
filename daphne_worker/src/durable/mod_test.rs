// Copyright (c) 2022 Cloudflare, Inc. All rights reserved.
// SPDX-License-Identifier: BSD-3-Clause

use crate::durable::{
    durable_name_agg_store, durable_name_queue, durable_name_report_store,
    report_id_hex_from_report,
};
use daphne::{
    messages::{Id, Report, ReportId, ReportMetadata},
    test_version, test_versions, DapBatchBucket, DapVersion,
};
use paste::paste;
use prio::codec::{ParameterizedDecode, ParameterizedEncode};
use rand::prelude::*;

#[test]
fn durable_name() {
    let time = 1664850074;
    let id1 = Id([17; 32]);
    let id2 = Id([34; 32]);
    let shard = 1234;

    assert_eq!(durable_name_queue(shard), "queue/1234");

    assert_eq!(
        durable_name_report_store(&DapVersion::Draft02, &id1.to_hex(), time, shard),
        "v02/task/1111111111111111111111111111111111111111111111111111111111111111/epoch/00000000001664850074/shard/1234",
    );

    assert_eq!(
        durable_name_agg_store(&DapVersion::Draft02, &id1.to_hex(), &DapBatchBucket::FixedSize{ batch_id: &id2 }),
        "v02/task/1111111111111111111111111111111111111111111111111111111111111111/batch/2222222222222222222222222222222222222222222222222222222222222222",
    );

    assert_eq!(
        durable_name_agg_store(&DapVersion::Draft02, &id1.to_hex(), &DapBatchBucket::TimeInterval{ batch_window: time }),
        "v02/task/1111111111111111111111111111111111111111111111111111111111111111/window/1664850074",
    );
}

// Test that the `report_id_from_report()` method properly extracts the report ID from the
// hex-encoded report. This helps ensure that changes to the `Report` wire format don't cause any
// regressions to `ReportStore`.
fn parse_report_id_hex_from_report(version: DapVersion) {
    let task_id = Id([17; 32]);
    let mut rng = thread_rng();
    let report = Report {
        task_id: if version == DapVersion::Draft02 {
            Some(task_id.clone())
        } else {
            None
        },
        report_metadata: ReportMetadata {
            id: ReportId(rng.gen()),
            time: rng.gen(),
            extensions: Vec::default(),
        },
        public_share: Vec::default(),
        encrypted_input_shares: Vec::default(),
    };

    let prefix = if version == DapVersion::Draft02 {
        "02"
    } else {
        // This is Draft04 and later
        "04"
    };
    let encoded_task_id = if version == DapVersion::Draft02 {
        "".to_string()
    } else {
        task_id.to_hex()
    };
    let report_hex = hex::encode(report.get_encoded_with_param(&version));
    let do_hex = format!("{}{}{}", prefix, encoded_task_id, report_hex);
    let key = report_id_hex_from_report(&do_hex).unwrap();
    assert_eq!(
        ReportId::get_decoded_with_param(&version, &hex::decode(key).unwrap()).unwrap(),
        report.report_metadata.id
    );
}

test_versions! {parse_report_id_hex_from_report}
