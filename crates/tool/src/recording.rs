// Copyright The eha-sdk Contributors

//! ToolSession 的本地、可审计记录器。

use std::{
    fs::{self, File},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};

pub(crate) struct Recording {
    id: String,
    directory: PathBuf,
    metadata_path: PathBuf,
    events: BufWriter<File>,
    telemetry: BufWriter<File>,
    metadata: Value,
    event_count: u64,
    telemetry_count: u64,
    dropped: u64,
    started_at: Instant,
}

impl Recording {
    pub(crate) fn start(root: &Path, metadata: Value) -> Result<Self, String> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_millis();
        fs::create_dir_all(root).map_err(|error| error.to_string())?;
        let id_base = format!("run-{stamp}-{}", std::process::id());
        let mut created = None;
        for index in 0_u32..10_000 {
            let id = if index == 0 {
                id_base.clone()
            } else {
                format!("{id_base}-{index}")
            };
            let directory = root.join(&id);
            match fs::create_dir(&directory) {
                Ok(()) => {
                    created = Some((id, directory));
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        let (id, directory) = created.ok_or_else(|| "无法为记录创建唯一目录".to_owned())?;
        let metadata_path = directory.join("metadata.json");
        let metadata_file = File::options()
            .write(true)
            .create_new(true)
            .open(&metadata_path)
            .map_err(|error| error.to_string())?;
        serde_json::to_writer_pretty(metadata_file, &metadata)
            .map_err(|error| error.to_string())?;

        let events = BufWriter::new(
            File::options()
                .write(true)
                .create_new(true)
                .open(directory.join("events.jsonl"))
                .map_err(|error| error.to_string())?,
        );
        let mut telemetry = BufWriter::new(
            File::options()
                .write(true)
                .create_new(true)
                .open(directory.join("telemetry.csv"))
                .map_err(|error| error.to_string())?,
        );
        writeln!(
            telemetry,
            "recorded_unix_time_ms,elapsed_ms,time_us,sequence,target_mode,target_values,position_mm,position_result,position_quality,position_stale,velocity_mm_s,velocity_result,velocity_quality,velocity_stale,pressure_a_mpa,pressure_a_result,pressure_a_quality,pressure_a_stale,pressure_b_mpa,pressure_b_result,pressure_b_quality,pressure_b_stale,force_n,force_result,force_quality,force_stale,gap,telemetry_json"
        )
        .map_err(|error| error.to_string())?;

        let mut recording = Self {
            id,
            directory,
            metadata_path,
            events,
            telemetry,
            metadata,
            event_count: 0,
            telemetry_count: 0,
            dropped: 0,
            started_at: Instant::now(),
        };
        recording.event("recording_started", json!({}))?;
        Ok(recording)
    }

    pub(crate) fn event(&mut self, kind: &str, data: Value) -> Result<(), String> {
        let event = json!({
            "event": kind,
            "unix_time_ms": unix_time_ms(),
            "elapsed_ms": self.started_at.elapsed().as_millis(),
            "data": data,
        });
        serde_json::to_writer(&mut self.events, &event).map_err(|error| error.to_string())?;
        self.events
            .write_all(b"\n")
            .map_err(|error| error.to_string())?;
        self.event_count = self.event_count.saturating_add(1);
        Ok(())
    }

    pub(crate) fn telemetry(&mut self, telemetry: &Value, gap: bool) -> Result<(), String> {
        let cell = |value: &Value| serde_json::to_string(value).unwrap_or_else(|_| "null".into());
        let main = telemetry["main_values"].as_array();
        let time_us = &telemetry["sample"]["snapshot_time_us"];
        let sequence = &telemetry["sample"]["snapshot_sequence"];
        let value_columns = |index| {
            let value = main
                .and_then(|values| values.get(index))
                .unwrap_or(&Value::Null);
            [
                value["value"]
                    .as_f64()
                    .map_or_else(String::new, |number| number.to_string()),
                value["result"]
                    .as_u64()
                    .map_or_else(String::new, |number| number.to_string()),
                value["quality"]
                    .as_u64()
                    .map_or_else(String::new, |number| number.to_string()),
                value["stale"]
                    .as_bool()
                    .map_or_else(String::new, |value| value.to_string()),
            ]
        };
        let mut line = vec![
            unix_time_ms().to_string(),
            self.started_at.elapsed().as_millis().to_string(),
            cell(time_us),
            cell(sequence),
            cell(&telemetry["target_mode"]),
            cell(&telemetry["target_values"]),
        ];
        for index in [0, 1, 2, 3, 4] {
            line.extend(value_columns(index));
        }
        line.extend([gap.to_string(), cell(telemetry)]);
        write_csv_row(&mut self.telemetry, &line).map_err(|error| error.to_string())?;
        self.telemetry_count = self.telemetry_count.saturating_add(1);
        Ok(())
    }

    pub(crate) fn dropped(&mut self, count: u64) -> Result<(), String> {
        if count == 0 {
            return Ok(());
        }
        self.dropped = self.dropped.saturating_add(count);
        self.event("telemetry_dropped", json!({"dropped": count}))
    }

    pub(crate) fn snapshot(&self) -> Value {
        json!({
            "active": true,
            "id": self.id,
            "directory": self.directory,
            "metadata": self.metadata,
            "events": self.event_count,
            "telemetry": self.telemetry_count,
            "dropped": self.dropped,
        })
    }

    pub(crate) fn flush(&mut self) -> Result<(), String> {
        self.events.flush().map_err(|error| error.to_string())?;
        self.telemetry.flush().map_err(|error| error.to_string())
    }

    pub(crate) fn finish(mut self) -> Result<Value, String> {
        self.event("recording_stopped", json!({}))?;
        self.flush()?;
        let result = json!({
            "active": false,
            "id": self.id,
            "directory": self.directory,
            "events": self.event_count,
            "telemetry": self.telemetry_count,
            "dropped": self.dropped,
            "ended_unix_time_ms": unix_time_ms(),
        });
        let mut metadata = self.metadata;
        metadata["completion"] = result.clone();
        let file = File::options()
            .write(true)
            .truncate(true)
            .open(&self.metadata_path)
            .map_err(|error| error.to_string())?;
        serde_json::to_writer_pretty(file, &metadata).map_err(|error| error.to_string())?;
        Ok(result)
    }
}

fn unix_time_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_millis())
}

fn write_csv_row(output: &mut impl Write, values: &[String]) -> std::io::Result<()> {
    for (index, value) in values.iter().enumerate() {
        if index != 0 {
            output.write_all(b",")?;
        }
        output.write_all(b"\"")?;
        output.write_all(value.replace('"', "\"\"").as_bytes())?;
        output.write_all(b"\"")?;
    }
    output.write_all(b"\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_runs_keep_numeric_values_and_quality_columns() {
        let root = std::env::temp_dir().join(format!("eha-tool-recording-{}", std::process::id()));
        let mut first =
            Recording::start(&root, json!({"device_uid":"a"})).expect("first recording");
        let second = Recording::start(&root, json!({"device_uid":"b"})).expect("second recording");
        assert_ne!(first.snapshot()["id"], second.snapshot()["id"]);
        first
            .telemetry(
                &json!({
                    "sample":{"snapshot_time_us": 7, "snapshot_sequence": 3},
                    "target_mode": 1,
                    "target_values": [2.5, 0.0, 0.0],
                    "main_values": [
                        {"value":2.5,"result":1,"quality":1,"stale":false},
                        {"value":0.5,"result":1,"quality":1,"stale":false},
                        {"value":1.2,"result":1,"quality":1,"stale":false},
                        {"value":1.3,"result":1,"quality":1,"stale":false},
                        {"value":9.0,"result":1,"quality":1,"stale":false}
                    ]
                }),
                false,
            )
            .expect("telemetry row");
        let first_result = first.finish().expect("first finish");
        let first_csv = fs::read_to_string(
            PathBuf::from(first_result["directory"].as_str().expect("directory"))
                .join("telemetry.csv"),
        )
        .expect("CSV");
        assert!(first_csv.contains("position_mm,position_result,position_quality,position_stale"));
        assert!(first_csv.contains("\"2.5\",\"1\",\"1\",\"false\""));
        let _ = second.finish();
        fs::remove_dir_all(root).expect("temporary recordings removed");
    }
}
