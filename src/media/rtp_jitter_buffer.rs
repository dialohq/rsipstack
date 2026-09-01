use std::collections::BTreeMap;

use bytes::Bytes;

const SEQUENCE_MODULUS: u64 = 1 << 16;
const HALF_SEQUENCE_SPACE: u64 = SEQUENCE_MODULUS / 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpJitterBufferConfig {
    pub reorder_packets: usize,
    pub max_concealment_duration: u32,
}

impl Default for RtpJitterBufferConfig {
    fn default() -> Self {
        Self {
            reorder_packets: 5,
            max_concealment_duration: 8_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtpJitterPacket {
    pub sequence_number: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub payload_type: u8,
    /// Duration in RTP timestamp units. Zero keeps sequence ordering without advancing the media clock.
    pub duration: u32,
    pub payload: Bytes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RtpJitterBufferOutput {
    Packet(RtpJitterPacket),
    Silence { timestamp: u32, duration: u32 },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RtpJitterBufferStats {
    pub received_packets: u64,
    pub emitted_packets: u64,
    pub reordered_packets: u64,
    pub duplicate_packets: u64,
    pub late_packets: u64,
    pub missing_packets: u64,
    pub concealed_duration: u64,
    pub timestamp_resets: u64,
    pub source_resets: u64,
}

pub struct RtpJitterBuffer {
    config: RtpJitterBufferConfig,
    ssrc: Option<u32>,
    packets: BTreeMap<u64, RtpJitterPacket>,
    next_sequence: Option<u64>,
    highest_sequence: Option<u64>,
    expected_timestamp: Option<u32>,
    stats: RtpJitterBufferStats,
}

impl RtpJitterBuffer {
    pub fn new(config: RtpJitterBufferConfig) -> Self {
        Self {
            config,
            ssrc: None,
            packets: BTreeMap::new(),
            next_sequence: None,
            highest_sequence: None,
            expected_timestamp: None,
            stats: RtpJitterBufferStats::default(),
        }
    }

    pub fn push(&mut self, packet: RtpJitterPacket) -> Vec<RtpJitterBufferOutput> {
        self.stats.received_packets += 1;
        let mut output = Vec::new();

        if self.ssrc.is_some_and(|ssrc| ssrc != packet.ssrc) {
            output.extend(self.flush());
            self.reset_source();
            self.stats.source_resets += 1;
        }
        self.ssrc = Some(packet.ssrc);

        let extended_sequence = self.extend_sequence(packet.sequence_number);
        if self
            .next_sequence
            .is_some_and(|next_sequence| extended_sequence < next_sequence)
        {
            self.stats.late_packets += 1;
            return output;
        }

        if self.packets.contains_key(&extended_sequence) {
            self.stats.duplicate_packets += 1;
            return output;
        }

        if self
            .highest_sequence
            .is_some_and(|highest_sequence| extended_sequence < highest_sequence)
        {
            self.stats.reordered_packets += 1;
        }

        self.highest_sequence = Some(
            self.highest_sequence
                .map_or(extended_sequence, |highest| highest.max(extended_sequence)),
        );
        self.packets.insert(extended_sequence, packet);
        self.drain(false, &mut output);
        output
    }

    pub fn flush(&mut self) -> Vec<RtpJitterBufferOutput> {
        let mut output = Vec::new();
        self.drain(true, &mut output);
        output
    }

    pub fn stats(&self) -> RtpJitterBufferStats {
        self.stats
    }

    fn drain(&mut self, flush: bool, output: &mut Vec<RtpJitterBufferOutput>) {
        while !self.packets.is_empty() {
            let first_sequence = *self.packets.first_key_value().unwrap().0;
            let next_sequence = self.next_sequence.unwrap_or(first_sequence);
            let sequence = if self.packets.contains_key(&next_sequence) {
                next_sequence
            } else if flush || self.packets.len() > self.config.reorder_packets {
                self.stats.missing_packets += first_sequence.saturating_sub(next_sequence);
                first_sequence
            } else {
                break;
            };

            let packet = self.packets.remove(&sequence).unwrap();
            self.emit(sequence, packet, output);
        }
    }

    fn emit(
        &mut self,
        sequence: u64,
        packet: RtpJitterPacket,
        output: &mut Vec<RtpJitterBufferOutput>,
    ) {
        if packet.duration > 0 {
            if let Some(expected_timestamp) = self.expected_timestamp {
                let gap = packet.timestamp.wrapping_sub(expected_timestamp);
                if gap > 0 && gap < (1 << 31) {
                    if gap <= self.config.max_concealment_duration {
                        output.push(RtpJitterBufferOutput::Silence {
                            timestamp: expected_timestamp,
                            duration: gap,
                        });
                        self.stats.concealed_duration += u64::from(gap);
                    } else {
                        self.stats.timestamp_resets += 1;
                    }
                } else if gap >= (1 << 31) {
                    self.stats.timestamp_resets += 1;
                }
            }
            self.expected_timestamp = Some(packet.timestamp.wrapping_add(packet.duration));
        }

        self.next_sequence = Some(sequence + 1);
        self.stats.emitted_packets += 1;
        output.push(RtpJitterBufferOutput::Packet(packet));
    }

    fn extend_sequence(&self, sequence_number: u16) -> u64 {
        let Some(reference) = self.highest_sequence.or(self.next_sequence) else {
            return u64::from(sequence_number);
        };

        let base = reference & !(SEQUENCE_MODULUS - 1);
        let mut candidate = base | u64::from(sequence_number);
        if candidate + HALF_SEQUENCE_SPACE < reference {
            candidate += SEQUENCE_MODULUS;
        } else if candidate > reference + HALF_SEQUENCE_SPACE && candidate >= SEQUENCE_MODULUS {
            candidate -= SEQUENCE_MODULUS;
        }
        candidate
    }

    fn reset_source(&mut self) {
        self.ssrc = None;
        self.packets.clear();
        self.next_sequence = None;
        self.highest_sequence = None;
        self.expected_timestamp = None;
    }
}

impl Default for RtpJitterBuffer {
    fn default() -> Self {
        Self::new(RtpJitterBufferConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(sequence_number: u16, timestamp: u32) -> RtpJitterPacket {
        RtpJitterPacket {
            sequence_number,
            timestamp,
            ssrc: 7,
            payload_type: 8,
            duration: 160,
            payload: Bytes::from(vec![sequence_number as u8; 160]),
        }
    }

    fn packet_sequences(output: &[RtpJitterBufferOutput]) -> Vec<u16> {
        output
            .iter()
            .filter_map(|output| match output {
                RtpJitterBufferOutput::Packet(packet) => Some(packet.sequence_number),
                RtpJitterBufferOutput::Silence { .. } => None,
            })
            .collect()
    }

    #[test]
    fn emits_ordered_packets_without_buffering() {
        let mut buffer = RtpJitterBuffer::new(RtpJitterBufferConfig {
            reorder_packets: 2,
            ..Default::default()
        });

        assert_eq!(packet_sequences(&buffer.push(packet(10, 0))), [10]);
        assert_eq!(packet_sequences(&buffer.push(packet(11, 160))), [11]);
        assert_eq!(packet_sequences(&buffer.push(packet(12, 320))), [12]);
        assert!(buffer.flush().is_empty());
    }

    #[test]
    fn reorders_packets_within_the_buffer() {
        let mut buffer = RtpJitterBuffer::new(RtpJitterBufferConfig {
            reorder_packets: 2,
            ..Default::default()
        });

        assert_eq!(packet_sequences(&buffer.push(packet(10, 0))), [10]);
        assert!(buffer.push(packet(12, 320)).is_empty());
        assert_eq!(packet_sequences(&buffer.push(packet(11, 160))), [11, 12]);
        assert!(buffer.flush().is_empty());
        assert_eq!(buffer.stats().reordered_packets, 1);
    }

    #[test]
    fn buffers_only_while_a_sequence_gap_is_open() {
        let mut buffer = RtpJitterBuffer::default();

        assert_eq!(packet_sequences(&buffer.push(packet(10, 0))), [10]);
        for sequence in 12..=16 {
            assert!(buffer
                .push(packet(sequence, u32::from(sequence - 10) * 160))
                .is_empty());
        }

        let output = buffer.push(packet(17, 1_120));
        assert_eq!(packet_sequences(&output), [12, 13, 14, 15, 16, 17]);
        assert_eq!(buffer.stats().missing_packets, 1);
        assert_eq!(buffer.stats().concealed_duration, 160);
    }

    #[test]
    fn establishes_sequence_from_the_first_observed_packet() {
        let mut buffer = RtpJitterBuffer::new(RtpJitterBufferConfig {
            reorder_packets: 2,
            ..Default::default()
        });

        assert_eq!(packet_sequences(&buffer.push(packet(12, 320))), [12]);
        assert!(buffer.push(packet(10, 0)).is_empty());
        assert_eq!(packet_sequences(&buffer.push(packet(13, 480))), [13]);
        assert_eq!(buffer.stats().late_packets, 1);
    }

    #[test]
    fn rejects_buffered_duplicates_and_emitted_packets() {
        let mut buffer = RtpJitterBuffer::new(RtpJitterBufferConfig {
            reorder_packets: 1,
            ..Default::default()
        });

        buffer.push(packet(10, 0));
        buffer.push(packet(12, 320));
        buffer.push(packet(12, 320));
        assert_eq!(packet_sequences(&buffer.push(packet(11, 160))), [11, 12]);
        buffer.push(packet(10, 0));

        assert_eq!(buffer.stats().duplicate_packets, 1);
        assert_eq!(buffer.stats().late_packets, 1);
        assert!(buffer.flush().is_empty());
    }

    #[test]
    fn rejects_late_packets() {
        let mut buffer = RtpJitterBuffer::new(RtpJitterBufferConfig {
            reorder_packets: 1,
            ..Default::default()
        });

        for sequence in 10..20 {
            buffer.push(packet(sequence, u32::from(sequence - 10) * 160));
        }
        buffer.push(packet(10, 0));

        assert_eq!(buffer.stats().late_packets, 1);
    }

    #[test]
    fn inserts_silence_for_missing_packet_timestamps() {
        let mut buffer = RtpJitterBuffer::new(RtpJitterBufferConfig {
            reorder_packets: 1,
            ..Default::default()
        });

        assert_eq!(packet_sequences(&buffer.push(packet(10, 0))), [10]);
        assert!(buffer.push(packet(12, 320)).is_empty());
        let output = buffer.push(packet(13, 480));

        assert_eq!(
            output,
            vec![
                RtpJitterBufferOutput::Silence {
                    timestamp: 160,
                    duration: 160,
                },
                RtpJitterBufferOutput::Packet(packet(12, 320)),
                RtpJitterBufferOutput::Packet(packet(13, 480)),
            ]
        );
        assert_eq!(buffer.stats().missing_packets, 1);
        assert_eq!(buffer.stats().concealed_duration, 160);
    }

    #[test]
    fn handles_sequence_and_timestamp_wraparound() {
        let mut buffer = RtpJitterBuffer::new(RtpJitterBufferConfig {
            reorder_packets: 1,
            ..Default::default()
        });

        assert_eq!(
            packet_sequences(&buffer.push(packet(u16::MAX, u32::MAX - 159))),
            [u16::MAX]
        );
        assert_eq!(packet_sequences(&buffer.push(packet(0, 0))), [0]);
        assert_eq!(packet_sequences(&buffer.push(packet(1, 160))), [1]);
        assert!(buffer.flush().is_empty());
        assert_eq!(buffer.stats().concealed_duration, 0);
    }

    #[test]
    fn zero_duration_packets_do_not_advance_the_media_clock() {
        let mut buffer = RtpJitterBuffer::new(RtpJitterBufferConfig {
            reorder_packets: 0,
            ..Default::default()
        });

        buffer.push(packet(10, 0));
        let mut telephone_event = packet(11, 160);
        telephone_event.payload_type = 101;
        telephone_event.duration = 0;
        buffer.push(telephone_event);
        let output = buffer.push(packet(12, 160));

        assert_eq!(packet_sequences(&output), [12]);
        assert_eq!(buffer.stats().concealed_duration, 0);
        assert_eq!(buffer.stats().timestamp_resets, 0);
    }

    #[test]
    fn resets_timestamp_after_unreasonable_gap() {
        let mut buffer = RtpJitterBuffer::new(RtpJitterBufferConfig {
            reorder_packets: 1,
            max_concealment_duration: 800,
        });

        buffer.push(packet(10, 0));
        buffer.push(packet(11, 160));
        let output = buffer.push(packet(12, 10_000));

        assert_eq!(packet_sequences(&output), [12]);
        assert_eq!(buffer.stats().timestamp_resets, 1);
        assert_eq!(buffer.stats().concealed_duration, 0);
    }

    #[test]
    fn flushes_and_resets_when_ssrc_changes() {
        let mut buffer = RtpJitterBuffer::new(RtpJitterBufferConfig {
            reorder_packets: 2,
            ..Default::default()
        });

        buffer.push(packet(10, 0));
        buffer.push(packet(12, 320));
        let mut new_source = packet(100, 5_000);
        new_source.ssrc = 9;
        let output = buffer.push(new_source);

        assert_eq!(packet_sequences(&output), [12, 100]);
        assert_eq!(buffer.stats().source_resets, 1);
        assert!(buffer.flush().is_empty());
    }
}
