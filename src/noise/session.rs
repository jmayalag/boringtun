// Copyright (c) 2019 Cloudflare, Inc. All rights reserved.
// SPDX-License-Identifier: BSD-3-Clause

use super::PacketData;
#[cfg(target_arch = "arm")]
use crate::crypto::chacha20poly1305::*;
use crate::noise::errors::WireGuardError;
#[cfg(not(target_arch = "arm"))]
use ring::aead::*;
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct Session {
    receiving_index: u32,
    sending_index: u32,
    #[cfg(not(target_arch = "arm"))]
    receiver: OpeningKey,
    #[cfg(not(target_arch = "arm"))]
    sender: SealingKey,
    #[cfg(target_arch = "arm")]
    receiver: ChaCha20Poly1305,
    #[cfg(target_arch = "arm")]
    sender: ChaCha20Poly1305,
    sending_key_counter: AtomicUsize,
    receiving_key_counter: spin::Mutex<ReceivingKeyCounterValidator>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "Session: {}<- ->{}",
            self.receiving_index, self.sending_index
        )
    }
}

const DATA_OFFSET: usize = 16; // Where encrypted data resides in a data packet
const AEAD_SIZE: usize = 16; // The overhead of the AEAE

// Receiving buffer constants
const WORD_SIZE: u64 = 64;
const N_WORDS: u64 = 16; // Suffice to reorder 64*16 = 1024 packets; can be increased at will
const N_BITS: u64 = WORD_SIZE * N_WORDS;

#[derive(Debug, Clone, Default)]
struct ReceivingKeyCounterValidator {
    // In order to avoid replays while allowing for some reordering of the packets, we keep a
    // bitmap of received packets, and the value of the highest counter
    next: u64,
    bitmap: [u64; N_WORDS as usize],
}

impl ReceivingKeyCounterValidator {
    #[inline(always)]
    fn set_bit(&mut self, idx: u64) {
        let bit_idx = idx % N_BITS;
        let word = (bit_idx / WORD_SIZE) as usize;
        let bit = (bit_idx % WORD_SIZE) as usize;
        self.bitmap[word] |= 1 << bit;
    }

    #[inline(always)]
    fn clear_bit(&mut self, idx: u64) {
        let bit_idx = idx % N_BITS;
        let word = (bit_idx / WORD_SIZE) as usize;
        let bit = (bit_idx % WORD_SIZE) as usize;
        self.bitmap[word] &= !(1u64 << bit);
    }

    // Clear the word that contains idx
    #[inline(always)]
    fn clear_word(&mut self, idx: u64) {
        let bit_idx = idx % N_BITS;
        let word = (bit_idx / WORD_SIZE) as usize;
        self.bitmap[word] = 0;
    }

    // Returns true if bit is set, false otherwise
    #[inline(always)]
    fn check_bit(&self, idx: u64) -> bool {
        let bit_idx = idx % N_BITS;
        let word = (bit_idx / WORD_SIZE) as usize;
        let bit = (bit_idx % WORD_SIZE) as usize;
        ((self.bitmap[word] >> bit) & 1) == 1
    }

    // Returns true if the counter was not yet received, and is not too far back
    #[inline(always)]
    fn will_accept(&self, counter: u64) -> bool {
        if counter >= self.next {
            // As long as the counter is growing no replay took place for sure
            return true;
        }
        if counter + N_BITS < self.next {
            // Drop if too far back
            return false;
        }
        !self.check_bit(counter)
    }

    // Marks the counter as received, and returns true if it is still good (in case during
    // decryption something changed)
    #[inline(always)]
    fn mark_did_receive(&mut self, counter: u64) -> Result<(), WireGuardError> {
        if counter + N_BITS < self.next {
            // Drop if too far back
            return Err(WireGuardError::InvalidCounter);
        }
        if counter == self.next {
            // Usually the packets arrive in order, in that case we simply mark the bit and
            // increment the counter
            self.set_bit(counter);
            self.next += 1;
            return Ok(());
        }
        if counter < self.next {
            // A packet arrived out of order, check if it is valid, and mark
            if self.check_bit(counter) {
                return Err(WireGuardError::InvalidCounter);
            }
            self.set_bit(counter);
            return Ok(());
        }
        // Packets where dropped, or maybe reordered, skip them and mark unused
        if counter - self.next >= N_BITS {
            // Too far ahead, clear all the bits
            for c in self.bitmap.iter_mut() {
                *c = 0;
            }
        } else {
            let mut i = self.next;
            while i % WORD_SIZE != 0 && i < counter {
                // Clear until i aligned to word size
                self.clear_bit(i);
                i += 1;
            }
            while i + WORD_SIZE < counter {
                // Clear whole word at a time
                self.clear_word(i);
                i = (i + WORD_SIZE) & 0u64.wrapping_sub(WORD_SIZE);
            }
            while i < counter {
                // Clear any remaining bits
                self.clear_bit(i);
                i += 1;
            }
        }
        self.set_bit(counter);
        self.next = counter + 1;
        Ok(())
    }
}

impl Session {
    pub(super) fn new(
        local_index: u32,
        peer_index: u32,
        receiving_key: [u8; 32],
        sending_key: [u8; 32],
    ) -> Session {
        Session {
            receiving_index: local_index,
            sending_index: peer_index,
            #[cfg(not(target_arch = "arm"))]
            receiver: OpeningKey::new(&CHACHA20_POLY1305, &receiving_key).unwrap(),
            #[cfg(not(target_arch = "arm"))]
            sender: SealingKey::new(&CHACHA20_POLY1305, &sending_key).unwrap(),
            #[cfg(target_arch = "arm")]
            receiver: ChaCha20Poly1305::new_aead(&receiving_key[..]),
            #[cfg(target_arch = "arm")]
            sender: ChaCha20Poly1305::new_aead(&sending_key[..]),
            sending_key_counter: AtomicUsize::new(0),
            receiving_key_counter: spin::Mutex::new(Default::default()),
        }
    }

    pub fn local_index(&self) -> usize {
        self.receiving_index as usize
    }

    // Returns true if receiving counter is good to use
    fn receiving_counter_quick_check(&self, counter: u64) -> Result<(), WireGuardError> {
        let counter_validator = self.receiving_key_counter.lock();
        if counter_validator.will_accept(counter) {
            Ok(())
        } else {
            Err(WireGuardError::InvalidCounter)
        }
    }

    // Returns true if receiving counter is good to use, and marks it as used {
    fn receiving_counter_mark(&self, counter: u64) -> Result<(), WireGuardError> {
        let mut counter_validator = self.receiving_key_counter.lock();
        counter_validator.mark_did_receive(counter)
    }

    // src - an IP packet from the interface
    // dst - pre-allocated space to hold the encapsulating UDP packet to send over the network
    // returns the size of the formatted packet
    pub(super) fn format_packet_data<'a>(&self, src: &[u8], dst: &'a mut [u8]) -> &'a mut [u8] {
        if dst.len() < src.len() + super::DATA_OVERHEAD_SZ {
            panic!("The destination buffer is too small");
        }

        let sending_key_counter = self.sending_key_counter.fetch_add(1, Ordering::Relaxed) as u64;

        let (message_type, rest) = dst.split_at_mut(4);
        let (receiver_index, rest) = rest.split_at_mut(4);
        let (counter, data) = rest.split_at_mut(8);

        message_type.copy_from_slice(&super::DATA.to_le_bytes());
        receiver_index.copy_from_slice(&self.sending_index.to_le_bytes());
        counter.copy_from_slice(&sending_key_counter.to_le_bytes());

        // TODO: spec requires padding to 16 bytes, but actually works fine without it
        #[cfg(not(target_arch = "arm"))]
        let n = {
            let mut nonce = [0u8; 12];
            nonce[4..12].copy_from_slice(&sending_key_counter.to_le_bytes());
            data[..src.len()].copy_from_slice(src);
            seal_in_place(
                &self.sender,
                Nonce::assume_unique_for_key(nonce),
                Aad::from(&[]),
                &mut data[..src.len() + AEAD_SIZE],
                AEAD_SIZE,
            )
            .unwrap()
        };

        #[cfg(target_arch = "arm")]
        let n = self.sender.seal_wg(
            sending_key_counter,
            &[],
            src,
            &mut data[..src.len() + AEAD_SIZE],
        );

        &mut dst[..DATA_OFFSET + n]
    }

    // packet - a data packet we received from the network
    // dst - pre-allocated space to hold the encapsulated IP packet, to send to the interface
    //       dst will always take less space than src
    // return the size of the encapsulated packet on success
    pub(super) fn receive_packet_data<'a>(
        &self,
        packet: PacketData,
        dst: &'a mut [u8],
    ) -> Result<&'a mut [u8], WireGuardError> {
        let ct_len = packet.encrypted_encapsulated_packet.len();
        if dst.len() < ct_len {
            // This is a very incorrect use of the library, therefore panic and not error
            panic!("The destination buffer is too small");
        }
        if packet.receiver_idx != self.receiving_index {
            return Err(WireGuardError::WrongIndex);
        }
        // Don't reuse counters, in case this is a replay attack we want to quickly check the counter without running expensive decryption
        self.receiving_counter_quick_check(packet.counter)?;

        #[cfg(not(target_arch = "arm"))]
        let ret = {
            let mut nonce = [0u8; 12];
            nonce[4..12].copy_from_slice(&packet.counter.to_le_bytes());
            dst[..ct_len].copy_from_slice(packet.encrypted_encapsulated_packet);
            open_in_place(
                &self.receiver,
                Nonce::assume_unique_for_key(nonce),
                Aad::from(&[]),
                0,
                &mut dst[..ct_len],
            )
            .map_err(|_| WireGuardError::InvalidAeadTag)?
        };

        #[cfg(target_arch = "arm")]
        let ret = self.receiver.open_wg(
            packet.counter,
            &[],
            packet.encrypted_encapsulated_packet,
            dst,
        )?;

        // After decryption is done, check counter again, and mark as received
        self.receiving_counter_mark(packet.counter)?;
        Ok(ret)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_replay_counter() {
        let mut c: ReceivingKeyCounterValidator = Default::default();

        assert!(c.mark_did_receive(0).is_ok());
        assert!(c.mark_did_receive(0).is_err());
        assert!(c.mark_did_receive(1).is_ok());
        assert!(c.mark_did_receive(1).is_err());
        assert!(c.mark_did_receive(63).is_ok());
        assert!(c.mark_did_receive(63).is_err());
        assert!(c.mark_did_receive(15).is_ok());
        assert!(c.mark_did_receive(15).is_err());

        for i in 64..N_BITS + 128 {
            assert!(c.mark_did_receive(i).is_ok());
            assert!(c.mark_did_receive(i).is_err());
        }

        assert!(c.mark_did_receive(N_BITS * 3).is_ok());
        for i in 0..=N_BITS * 2 {
            assert!(!c.will_accept(i));
            assert!(c.mark_did_receive(i).is_err());
        }
        for i in N_BITS * 2 + 1..N_BITS * 3 {
            assert!(c.will_accept(i));
        }

        for i in (N_BITS * 2 + 1..N_BITS * 3).rev() {
            assert!(c.mark_did_receive(i).is_ok());
            assert!(c.mark_did_receive(i).is_err());
        }

        assert!(c.mark_did_receive(N_BITS * 3 + 70).is_ok());
        assert!(c.mark_did_receive(N_BITS * 3 + 71).is_ok());
        assert!(c.mark_did_receive(N_BITS * 3 + 72).is_ok());
        assert!(c.mark_did_receive(N_BITS * 3 + 72 + 125).is_ok());
        assert!(c.mark_did_receive(N_BITS * 3 + 63).is_ok());

        assert!(c.mark_did_receive(N_BITS * 3 + 70).is_err());
        assert!(c.mark_did_receive(N_BITS * 3 + 71).is_err());
        assert!(c.mark_did_receive(N_BITS * 3 + 72).is_err());
    }
}
