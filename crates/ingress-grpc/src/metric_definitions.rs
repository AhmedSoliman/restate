// Copyright (c) 2023 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

/// Optional to have but adds description/help message to the metrics emitted to
/// the metrics' sink.
use metrics::{describe_histogram, Unit};

pub(crate) fn describe_metrics() {
    describe_histogram!(
        "ingress.duration.seconds",
        Unit::Seconds,
        "Total latency of Ingress request processing in seconds"
    );
}
