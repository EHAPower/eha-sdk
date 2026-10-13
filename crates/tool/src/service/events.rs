// Copyright The eha-sdk Contributors
//! owner 提供事实的有界缓存；不持有设备、Client、查询计划或消费者状态。

use serde::{Serialize, Serializer};
use serde_json::value::RawValue;
use std::{
    collections::VecDeque,
    mem::size_of,
    time::{Duration, Instant},
};

/// 容量由服务选择并在 describe 声明；字节预算包含原文和事件固定开销。
#[derive(Clone, Copy, Debug)]
pub(super) struct Limits {
    pub max_events: usize,
    pub max_bytes: usize,
    pub max_batch: usize,
    pub retention: Duration,
}

/// 来源时钟由采集者明确选择，不能把主机接收时间冒充设备采样时间。
#[derive(Debug, Serialize)]
#[serde(tag = "clock", content = "value")]
pub(super) enum SourceTimestamp {
    #[serde(rename = "device_monotonic_us")]
    DeviceMonotonicUs(#[serde(serialize_with = "decimal")] u64),
    #[serde(rename = "host_unix_ms")]
    HostUnixMs(#[serde(serialize_with = "decimal")] u64),
}

/// 设备来源的缺口原因与已知数量；未知数量不从服务 cursor 差值推断。
#[derive(Debug, Serialize)]
pub(super) struct SourceGap {
    pub reason: &'static str,
    #[serde(serialize_with = "optional_decimal")]
    pub dropped: Option<u64>,
}

pub(super) struct EventInput {
    pub kind: &'static str,
    pub source_id: String,
    pub source_timestamp: SourceTimestamp,
    pub received_at: u64,
    /// 只投影原始 JSON；本模块不将显示 JSON 解码为业务值。
    pub payload: Box<RawValue>,
    pub source_gap: Option<SourceGap>,
}

#[derive(Debug, Serialize)]
pub(super) struct Event {
    #[serde(serialize_with = "decimal")]
    pub cursor: u64,
    pub kind: &'static str,
    pub source_id: Box<str>,
    pub source_timestamp: SourceTimestamp,
    /// 主机 UNIX 毫秒；保留策略使用 Instant，与此墙钟独立。
    #[serde(serialize_with = "decimal")]
    pub received_at: u64,
    pub payload: Box<RawValue>,
    pub gap: bool,
    pub source_gap: Option<SourceGap>,
}

struct StoredEvent {
    event: Event,
    received: Instant,
    bytes: usize,
}

#[derive(Debug, Serialize)]
pub(super) struct LostRange {
    #[serde(serialize_with = "decimal")]
    pub first_cursor: u64,
    #[serde(serialize_with = "decimal")]
    pub last_cursor: u64,
    #[serde(serialize_with = "decimal")]
    pub count: u64,
}

#[derive(Debug, Serialize)]
pub(super) struct Batch<'a> {
    pub events: Vec<&'a Event>,
    /// 最后实际返回的序号；空批次为当前水位，绝不因 limit 跳过未返回事件。
    #[serde(serialize_with = "decimal")]
    pub cursor: u64,
    pub gap: bool,
    pub gap_reasons: Vec<&'static str>,
    /// 仅服务缓存的确定丢失区间，不代表设备掉样数。
    pub lost_ranges: Vec<LostRange>,
    pub truncated: bool,
}

pub(super) struct EventLog {
    limits: Limits,
    events: VecDeque<StoredEvent>,
    bytes: usize,
    latest_cursor: u64,
}

impl EventLog {
    pub fn new(limits: Limits) -> Result<Self, &'static str> {
        if limits.max_events == 0
            || limits.max_bytes == 0
            || limits.max_batch == 0
            || limits.retention.is_zero()
        {
            return Err("事件缓存上限和保留期必须大于零");
        }
        Ok(Self {
            limits,
            events: VecDeque::new(),
            bytes: 0,
            latest_cursor: 0,
        })
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// now 必须来自 owner 的主机单调时钟，不使用设备时钟或浏览器时间控制保留期。
    /// 超大事实也分配序号并返回错误，使后续读取能看见确定的服务缓存丢失。
    pub fn append(&mut self, now: Instant, input: EventInput) -> Result<u64, &'static str> {
        let cursor = self
            .latest_cursor
            .checked_add(1)
            .ok_or("事件 cursor 已耗尽")?;
        self.latest_cursor = cursor;
        self.expire(now);
        let bytes = size_of::<StoredEvent>()
            .checked_add(input.source_id.len())
            .and_then(|bytes| bytes.checked_add(input.payload.get().len()))
            .and_then(|bytes| bytes.checked_add(input.kind.len()))
            .and_then(|bytes| {
                bytes.checked_add(input.source_gap.as_ref().map_or(0, |gap| gap.reason.len()))
            })
            .ok_or("事件超过缓存字节上限")?;
        if bytes > self.limits.max_bytes {
            return Err("事件超过缓存字节上限");
        }
        while self.events.len() >= self.limits.max_events
            || self.bytes > self.limits.max_bytes - bytes
        {
            self.pop_front();
        }
        self.events.push_back(StoredEvent {
            event: Event {
                cursor,
                kind: input.kind,
                source_id: input.source_id.into_boxed_str(),
                source_timestamp: input.source_timestamp,
                received_at: input.received_at,
                payload: input.payload,
                gap: input.source_gap.is_some(),
                source_gap: input.source_gap,
            },
            received: now,
            bytes,
        });
        self.bytes += bytes;
        Ok(cursor)
    }

    /// after 为最后消费的序号。读取只按时间淘汰过期项，不记录或推进任何消费者状态。
    /// 超前 cursor 返回空批次、当前水位和 cursor_ahead，调用方须重新取得快照。
    pub fn batch(
        &mut self,
        now: Instant,
        after: Option<u64>,
        limit: usize,
    ) -> Result<Batch<'_>, &'static str> {
        if limit == 0 || limit > self.limits.max_batch {
            return Err("事件 limit 超出 describe 声明的范围");
        }
        self.expire(now);
        let after = after.unwrap_or(0);
        if after > self.latest_cursor {
            return Ok(Batch {
                events: Vec::new(),
                cursor: self.latest_cursor,
                gap: true,
                gap_reasons: vec!["cursor_ahead"],
                lost_ranges: Vec::new(),
                truncated: false,
            });
        }
        let mut matching = self
            .events
            .iter()
            .filter(|stored| stored.event.cursor > after);
        let events: Vec<_> = matching
            .by_ref()
            .take(limit)
            .map(|stored| &stored.event)
            .collect();
        let truncated = matching.next().is_some();
        let cursor = events
            .last()
            .map_or(self.latest_cursor, |event| event.cursor);
        let lost_ranges = self.lost_ranges(after, cursor);
        let mut gap_reasons = Vec::new();
        if !lost_ranges.is_empty() {
            gap_reasons.push("cache_loss");
        }
        if events.iter().any(|event| event.gap) {
            gap_reasons.push("source_gap");
        }
        Ok(Batch {
            events,
            cursor,
            gap: !gap_reasons.is_empty(),
            gap_reasons,
            lost_ranges,
            truncated,
        })
    }

    fn expire(&mut self, now: Instant) {
        while self.events.front().is_some_and(|stored| {
            now.saturating_duration_since(stored.received) >= self.limits.retention
        }) {
            self.pop_front();
        }
    }

    fn pop_front(&mut self) {
        if let Some(stored) = self.events.pop_front() {
            self.bytes -= stored.bytes;
        }
    }

    fn lost_ranges(&self, after: u64, through: u64) -> Vec<LostRange> {
        let mut ranges = Vec::new();
        let mut previous = after;
        for stored in self
            .events
            .iter()
            .filter(|stored| stored.event.cursor > after && stored.event.cursor <= through)
        {
            let cursor = stored.event.cursor;
            if cursor - previous > 1 {
                ranges.push(LostRange {
                    first_cursor: previous + 1,
                    last_cursor: cursor - 1,
                    count: cursor - previous - 1,
                });
            }
            previous = cursor;
        }
        if previous < through {
            ranges.push(LostRange {
                first_cursor: previous + 1,
                last_cursor: through,
                count: through - previous,
            });
        }
        ranges
    }
}

fn decimal<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&value.to_string())
}

fn optional_decimal<S: Serializer>(value: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error> {
    match value {
        Some(value) => serializer.serialize_some(&value.to_string()),
        None => serializer.serialize_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            max_events: 4,
            max_bytes: 4096,
            max_batch: 2,
            retention: Duration::from_secs(10),
        }
    }

    fn input() -> EventInput {
        EventInput {
            kind: "telemetry",
            source_id: "usb:test:g1".into(),
            source_timestamp: SourceTimestamp::DeviceMonotonicUs(9_007_199_254_740_993),
            received_at: 1_800_000_000_000,
            payload: RawValue::from_string("{\"position\":0.100000001490116119384765625}".into())
                .expect("valid event fixture"),
            source_gap: None,
        }
    }

    #[test]
    fn independent_consumers_and_limited_batches_do_not_skip() {
        let now = Instant::now();
        let mut log = EventLog::new(limits()).expect("valid event fixture");
        for _ in 0..3 {
            log.append(now, input()).expect("valid event fixture");
        }
        for _ in 0..2 {
            let batch = log.batch(now, None, 2).expect("valid event fixture");
            assert_eq!(
                batch
                    .events
                    .iter()
                    .map(|event| event.cursor)
                    .collect::<Vec<_>>(),
                [1, 2]
            );
            assert_eq!(batch.cursor, 2);
            assert!(batch.truncated);
            assert!(!batch.gap);
        }
        let batch = log.batch(now, Some(2), 2).expect("valid event fixture");
        assert_eq!(batch.events[0].cursor, 3);
        assert!(!batch.truncated);
        assert_eq!(
            log.batch(now, Some(3), 2)
                .expect("valid event fixture")
                .cursor,
            3
        );
    }

    #[test]
    fn cache_capacity_and_retention_report_exact_lost_ranges() {
        let now = Instant::now();
        let mut log = EventLog::new(Limits {
            max_events: 2,
            ..limits()
        })
        .expect("valid event fixture");
        for _ in 0..4 {
            log.append(now, input()).expect("valid event fixture");
        }
        let batch = log.batch(now, Some(0), 2).expect("valid event fixture");
        assert_eq!(batch.gap_reasons, ["cache_loss"]);
        assert_eq!(batch.lost_ranges[0].first_cursor, 1);
        assert_eq!(batch.lost_ranges[0].last_cursor, 2);
        assert_eq!(batch.lost_ranges[0].count, 2);
        assert!(!log.batch(now, Some(2), 2).expect("valid event fixture").gap);
        let batch = log
            .batch(now + Duration::from_secs(10), Some(2), 2)
            .expect("valid event fixture");
        assert!(batch.events.is_empty());
        assert_eq!(batch.cursor, 4);
        assert_eq!(batch.lost_ranges[0].first_cursor, 3);
        assert_eq!(batch.lost_ranges[0].count, 2);
        assert_eq!(log.bytes, 0);
    }

    #[test]
    fn metadata_and_payload_share_byte_budget_and_rejected_fact_leaves_gap() {
        let now = Instant::now();
        let event_bytes = size_of::<StoredEvent>()
            + input().source_id.len()
            + input().payload.get().len()
            + input().kind.len();
        let mut log = EventLog::new(Limits {
            max_bytes: event_bytes,
            ..limits()
        })
        .expect("valid event fixture");
        log.append(now, input()).expect("valid event fixture");
        let mut large = input();
        large.source_id.push('x');
        assert!(log.append(now, large).is_err());
        assert_eq!(log.bytes, event_bytes);
        assert_eq!(log.append(now, input()).expect("valid event fixture"), 3);
        let batch = log.batch(now, Some(1), 2).expect("valid event fixture");
        assert_eq!(batch.events[0].cursor, 3);
        assert_eq!(batch.lost_ranges[0].first_cursor, 2);
        assert_eq!(batch.lost_ranges[0].count, 1);
        assert!(log.bytes <= log.limits().max_bytes);
    }

    #[test]
    fn future_cursor_is_explicit_and_does_not_reset_or_replay() {
        let now = Instant::now();
        let mut log = EventLog::new(limits()).expect("valid event fixture");
        log.append(now, input()).expect("valid event fixture");
        let batch = log
            .batch(now, Some(u64::MAX), 2)
            .expect("valid event fixture");
        assert!(batch.events.is_empty());
        assert_eq!(batch.cursor, 1);
        assert_eq!(batch.gap_reasons, ["cursor_ahead"]);
        assert!(batch.lost_ranges.is_empty());
        assert!(log.batch(now, Some(0), 0).is_err());
        assert!(log.batch(now, Some(0), 3).is_err());
    }

    #[test]
    fn source_gap_clock_and_original_numbers_are_projection_only() {
        let now = Instant::now();
        let mut log = EventLog::new(limits()).expect("valid event fixture");
        log.latest_cursor = 9_007_199_254_740_992;
        let mut sample = input();
        sample.source_gap = Some(SourceGap {
            reason: "source_reset",
            dropped: None,
        });
        log.append(now, sample).expect("valid event fixture");
        let batch = log
            .batch(now, Some(9_007_199_254_740_992), 2)
            .expect("valid event fixture");
        assert_eq!(batch.gap_reasons, ["source_gap"]);
        assert!(batch.lost_ranges.is_empty());
        let text = serde_json::to_string(&batch).expect("valid event fixture");
        assert!(text.contains("\"cursor\":\"9007199254740993\""));
        assert!(text.contains("\"clock\":\"device_monotonic_us\",\"value\":\"9007199254740993\""));
        assert!(text.contains(input().payload.get()));
        assert!(text.contains("\"dropped\":null"));
        let mut host = input();
        host.kind = "connection";
        host.source_timestamp = SourceTimestamp::HostUnixMs(host.received_at);
        log.append(now, host).expect("valid event fixture");
        let text = serde_json::to_string(
            &log.batch(now, Some(9_007_199_254_740_993), 2)
                .expect("valid event fixture"),
        )
        .expect("valid event fixture");
        assert!(text.contains("\"clock\":\"host_unix_ms\""));
    }

    #[test]
    fn cursor_exhaustion_never_reuses_or_saturates_sequence() {
        let now = Instant::now();
        let mut log = EventLog::new(limits()).expect("valid event fixture");
        log.latest_cursor = u64::MAX;
        assert_eq!(
            log.append(now, input()).expect_err("invalid event fixture"),
            "事件 cursor 已耗尽"
        );
        assert_eq!(log.latest_cursor, u64::MAX);
        assert!(
            EventLog::new(Limits {
                max_events: 0,
                ..limits()
            })
            .is_err()
        );
    }
}
