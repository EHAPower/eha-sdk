// Copyright The eha-sdk Contributors

use protocol::{
    Direction, Error, ExpectedIdentity, ExpectedIdentityFields, Message, Response, SampleData,
    decode, encode,
    responses::{
        MeasurementFields, MeasurementValues, ModelState, PositiveForceChannel, SourceQuality,
        ValueFields, ValueResult, ValueState, encode_measurements,
    },
};

fn available(value: f32) -> ValueFields {
    ValueFields::new(
        value,
        ValueState::new(ValueResult::Available, SourceQuality::Qualified, false),
    )
}

fn main() -> Result<(), Error> {
    let sample = SampleData {
        query_id: 7,
        run_nonce: [1; 16],
        snapshot_sequence: 1,
        snapshot_time_us: 100,
    };
    let measurements = MeasurementFields {
        sample,
        values: MeasurementValues {
            position_mm: available(10.0),
            velocity_mm_s: ValueFields::new(
                0.0,
                ValueState::new(
                    ValueResult::HistoryInsufficient,
                    SourceQuality::Qualified,
                    false,
                ),
            ),
            raw_pressure_a_mpa: ValueFields::new(
                0.0,
                ValueState::new(
                    ValueResult::CalculationFailed,
                    SourceQuality::Faulted,
                    false,
                ),
            ),
            raw_pressure_b_mpa: available(2.0),
            filtered_pressure_a_mpa: ValueFields::new(
                1.0,
                ValueState::new(ValueResult::Available, SourceQuality::Qualified, true),
            ),
            filtered_pressure_b_mpa: available(2.0),
            main_force_n: available(3.0),
            protection_force_n: available(3.0),
        },
        position_age_us: 1,
        velocity_age_us: 1,
        pressure_pair_age_us: 1,
        position_reference_count: 42,
        effective_area_mm2: 10.0,
        positive_force_channel: PositiveForceChannel::ElectricalA,
        reference_state: true,
        model_state: ModelState::Available,
        pressure_batch_sequence: 1,
    };
    let mut measurement_bytes = [0u8; 108];
    encode_measurements(&measurements, &mut measurement_bytes)?;
    if let Message::Response(Response::Measurements(view)) =
        decode(&measurement_bytes, Direction::FirmwareToHost)?
    {
        let values = view.fields().values;
        assert_eq!(values.position_mm, available(10.0));
        assert_eq!(values.velocity_mm_s.value, 0.0);
        assert_eq!(
            values.velocity_mm_s.state.result(),
            ValueResult::HistoryInsufficient
        );
        assert_eq!(
            values.velocity_mm_s.state.quality(),
            SourceQuality::Qualified
        );
        assert!(!values.velocity_mm_s.state.is_stale());
        assert_eq!(values.raw_pressure_a_mpa.value, 0.0);
        assert_eq!(
            values.raw_pressure_a_mpa.state.result(),
            ValueResult::CalculationFailed
        );
        assert_eq!(
            values.raw_pressure_a_mpa.state.quality(),
            SourceQuality::Faulted
        );
        assert!(!values.raw_pressure_a_mpa.state.is_stale());
        assert_eq!(values.filtered_pressure_a_mpa.value, 1.0);
        assert_eq!(
            values.filtered_pressure_a_mpa.state.result(),
            ValueResult::Available
        );
        assert_eq!(
            values.filtered_pressure_a_mpa.state.quality(),
            SourceQuality::Qualified
        );
        assert!(values.filtered_pressure_a_mpa.state.is_stale());
        println!(
            "decoded measurement states: velocity=({:?}, {:?}, stale={}), pressure_a=({:?}, {:?}, stale={}), filtered_a=({:?}, {:?}, stale={})",
            values.velocity_mm_s.state.result(),
            values.velocity_mm_s.state.quality(),
            values.velocity_mm_s.state.is_stale(),
            values.raw_pressure_a_mpa.state.result(),
            values.raw_pressure_a_mpa.state.quality(),
            values.raw_pressure_a_mpa.state.is_stale(),
            values.filtered_pressure_a_mpa.state.result(),
            values.filtered_pressure_a_mpa.state.quality(),
            values.filtered_pressure_a_mpa.state.is_stale(),
        );
    }

    let identity_bytes = ExpectedIdentityFields::new([1; 12], [2; 16]).to_bytes();
    let identity = ExpectedIdentity::new(&identity_bytes).expect("固定28字节期望身份");
    let mut wire = [0u8; 64];
    let _length = encode(
        Message::SaveAndRestart {
            query_id: 1,
            identity,
            record: br#"{"format_version":1}"#,
        },
        &mut wire,
    )?;
    Ok(())
}
