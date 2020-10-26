// Copyright 2017-2020 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

// TODO: remove this module; we don't want to tie things together too much

use parity_scale_codec::{
    Decode, Encode, EncodeAsRef, EncodeLike, Error, HasCompact, Input, Output,
};
use primitive_types::H256;

/// Simple blob to hold an extrinsic without committing to its format and ensure it is serialized
/// correctly.
#[derive(Debug, PartialEq, Eq, Clone, Default, Encode, Decode)]
pub struct Extrinsic(pub Vec<u8>);

/// Hash of a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockHash(pub [u8; 32]);

/// Header of a block for a substrate chain.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Header {
    /// The parent hash.
    pub parent_hash: H256,
    /// The block number.
    pub number: u64,
    /// The state trie merkle root
    pub state_root: H256,
    /// The merkle root of the extrinsics.
    pub extrinsics_root: H256,
    /// A chain-specific digest of data useful for light clients or referencing auxiliary data.
    pub digest: Digest<H256>,
}

impl Header {
    /// Returns the hash of the header, and thus also of the block.
    pub fn block_hash(&self) -> BlockHash {
        let mut hasher = blake2_rfc::blake2b::Blake2b::with_key(32, &[]);
        hasher.update(&self.encode());
        let result = hasher.finalize();
        debug_assert_eq!(result.as_bytes().len(), 32);

        let mut out = [0; 32];
        out.copy_from_slice(result.as_bytes());
        BlockHash(out)
    }
}

impl Decode for Header {
    fn decode<I: Input>(input: &mut I) -> Result<Self, Error> {
        Ok(Header {
            parent_hash: Decode::decode(input)?,
            number: <<u64 as HasCompact>::Type>::decode(input)?.into(),
            state_root: Decode::decode(input)?,
            extrinsics_root: Decode::decode(input)?,
            digest: Decode::decode(input)?,
        })
    }
}

impl Encode for Header {
    fn encode_to<T: Output>(&self, dest: &mut T) {
        dest.push(&self.parent_hash);
        dest.push(&<<<u64 as HasCompact>::Type as EncodeAsRef<_>>::RefType>::from(&self.number));
        dest.push(&self.state_root);
        dest.push(&self.extrinsics_root);
        dest.push(&self.digest);
    }
}

/// Generic header digest.
#[derive(Debug, PartialEq, Eq, Clone, Encode, Decode)]
pub struct Digest<Hash: Encode + Decode> {
    /// A list of logs in the digest.
    pub logs: Vec<DigestItem<Hash>>,
}

/// Digest item that is able to encode/decode 'system' digest items and
/// provide opaque access to other items.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum DigestItem<Hash> {
    /// System digest item that contains the root of changes trie at given
    /// block. It is created for every block iff runtime supports changes
    /// trie creation.
    ChangesTrieRoot(Hash),

    /// A pre-runtime digest.
    ///
    /// These are messages from the consensus engine to the runtime, although
    /// the consensus engine can (and should) read them itself to avoid
    /// code and state duplication. It is erroneous for a runtime to produce
    /// these, but this is not (yet) checked.
    ///
    /// NOTE: the runtime is not allowed to panic or fail in an `on_initialize`
    /// call if an expected `PreRuntime` digest is not present. It is the
    /// responsibility of a external block verifier to check this. Runtime API calls
    /// will initialize the block without pre-runtime digests, so initialization
    /// cannot fail when they are missing.
    PreRuntime([u8; 4], Vec<u8>),

    /// A message from the runtime to the consensus engine. This should *never*
    /// be generated by the native code of any consensus engine, but this is not
    /// checked (yet).
    Consensus([u8; 4], Vec<u8>),

    /// Put a Seal on it. This is only used by native code, and is never seen
    /// by runtimes.
    Seal([u8; 4], Vec<u8>),

    /// Digest item that contains signal from changes tries manager to the
    /// native code.
    ChangesTrieSignal(ChangesTrieSignal),

    /// Some other thing. Unsupported and experimental.
    Other(Vec<u8>),
}

impl<Hash> DigestItem<Hash> {
    /// Returns a 'referencing view' for this digest item.
    pub fn dref<'a>(&'a self) -> DigestItemRef<'a, Hash> {
        match *self {
            DigestItem::ChangesTrieRoot(ref v) => DigestItemRef::ChangesTrieRoot(v),
            DigestItem::PreRuntime(ref v, ref s) => DigestItemRef::PreRuntime(v, s),
            DigestItem::Consensus(ref v, ref s) => DigestItemRef::Consensus(v, s),
            DigestItem::Seal(ref v, ref s) => DigestItemRef::Seal(v, s),
            DigestItem::ChangesTrieSignal(ref s) => DigestItemRef::ChangesTrieSignal(s),
            DigestItem::Other(ref v) => DigestItemRef::Other(v),
        }
    }
}

/// A 'referencing view' for digest item. Does not own its contents. Used by
/// final runtime implementations for encoding/decoding its log items.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum DigestItemRef<'a, Hash: 'a> {
    /// Reference to `DigestItem::ChangesTrieRoot`.
    ChangesTrieRoot(&'a Hash),
    /// A pre-runtime digest.
    ///
    /// These are messages from the consensus engine to the runtime, although
    /// the consensus engine can (and should) read them itself to avoid
    /// code and state duplication.  It is erroneous for a runtime to produce
    /// these, but this is not (yet) checked.
    PreRuntime(&'a [u8; 4], &'a Vec<u8>),
    /// A message from the runtime to the consensus engine. This should *never*
    /// be generated by the native code of any consensus engine, but this is not
    /// checked (yet).
    Consensus(&'a [u8; 4], &'a Vec<u8>),
    /// Put a Seal on it. This is only used by native code, and is never seen
    /// by runtimes.
    Seal(&'a [u8; 4], &'a Vec<u8>),
    /// Digest item that contains signal from changes tries manager to the
    /// native code.
    ChangesTrieSignal(&'a ChangesTrieSignal),
    /// Any 'non-system' digest item, opaque to the native code.
    Other(&'a Vec<u8>),
}

impl<'a, Hash: Encode> Encode for DigestItemRef<'a, Hash> {
    fn encode(&self) -> Vec<u8> {
        let mut v = Vec::new();

        match *self {
            DigestItemRef::ChangesTrieRoot(changes_trie_root) => {
                DigestItemType::ChangesTrieRoot.encode_to(&mut v);
                changes_trie_root.encode_to(&mut v);
            }
            DigestItemRef::Consensus(val, data) => {
                DigestItemType::Consensus.encode_to(&mut v);
                (val, data).encode_to(&mut v);
            }
            DigestItemRef::Seal(val, sig) => {
                DigestItemType::Seal.encode_to(&mut v);
                (val, sig).encode_to(&mut v);
            }
            DigestItemRef::PreRuntime(val, data) => {
                DigestItemType::PreRuntime.encode_to(&mut v);
                (val, data).encode_to(&mut v);
            }
            DigestItemRef::ChangesTrieSignal(changes_trie_signal) => {
                DigestItemType::ChangesTrieSignal.encode_to(&mut v);
                changes_trie_signal.encode_to(&mut v);
            }
            DigestItemRef::Other(val) => {
                DigestItemType::Other.encode_to(&mut v);
                val.encode_to(&mut v);
            }
        }

        v
    }
}

impl<'a, Hash: Encode> EncodeLike for DigestItemRef<'a, Hash> {}

impl<Hash: Encode> Encode for DigestItem<Hash> {
    fn encode(&self) -> Vec<u8> {
        self.dref().encode()
    }
}

impl<Hash: Encode> EncodeLike for DigestItem<Hash> {}

impl<Hash: Decode> Decode for DigestItem<Hash> {
    #[allow(deprecated)]
    fn decode<I: Input>(input: &mut I) -> Result<Self, Error> {
        let item_type: DigestItemType = Decode::decode(input)?;
        match item_type {
            DigestItemType::ChangesTrieRoot => {
                Ok(DigestItem::ChangesTrieRoot(Decode::decode(input)?))
            }
            DigestItemType::PreRuntime => {
                let vals: ([u8; 4], Vec<u8>) = Decode::decode(input)?;
                Ok(DigestItem::PreRuntime(vals.0, vals.1))
            }
            DigestItemType::Consensus => {
                let vals: ([u8; 4], Vec<u8>) = Decode::decode(input)?;
                Ok(DigestItem::Consensus(vals.0, vals.1))
            }
            DigestItemType::Seal => {
                let vals: ([u8; 4], Vec<u8>) = Decode::decode(input)?;
                Ok(DigestItem::Seal(vals.0, vals.1))
            }
            DigestItemType::ChangesTrieSignal => {
                Ok(DigestItem::ChangesTrieSignal(Decode::decode(input)?))
            }
            DigestItemType::Other => Ok(DigestItem::Other(Decode::decode(input)?)),
        }
    }
}

/// Type of the digest item. Used to gain explicit control over `DigestItem` encoding
/// process. We need an explicit control, because final runtimes are encoding their own
/// digest items using `DigestItemRef` type and we can't auto-derive `Decode`
/// trait for `DigestItemRef`.
#[repr(u32)]
#[derive(Encode, Decode)]
pub enum DigestItemType {
    Other = 0,
    ChangesTrieRoot = 2,
    Consensus = 4,
    Seal = 5,
    PreRuntime = 6,
    ChangesTrieSignal = 7,
}

/// Available changes trie signals.
#[derive(Debug, PartialEq, Eq, Clone, Encode, Decode)]
pub enum ChangesTrieSignal {
    /// New changes trie configuration is enacted, starting from **next block**.
    ///
    /// The block that emits this signal will contain changes trie (CT) that covers
    /// blocks range [BEGIN; current block], where BEGIN is (order matters):
    /// - LAST_TOP_LEVEL_DIGEST_BLOCK+1 if top level digest CT has ever been created
    ///   using current configuration AND the last top level digest CT has been created
    ///   at block LAST_TOP_LEVEL_DIGEST_BLOCK;
    /// - LAST_CONFIGURATION_CHANGE_BLOCK+1 if there has been CT configuration change
    ///   before and the last configuration change happened at block
    ///   LAST_CONFIGURATION_CHANGE_BLOCK;
    /// - 1 otherwise.
    NewConfiguration(Option<ChangesTrieConfiguration>),
}

/// Substrate changes trie configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default, Encode, Decode)]
pub struct ChangesTrieConfiguration {
    /// Interval (in blocks) at which level1-digests are created. Digests are not
    /// created when this is less or equal to 1.
    pub digest_interval: u32,
    /// Maximal number of digest levels in hierarchy. 0 means that digests are not
    /// created at all (even level1 digests). 1 means only level1-digests are created.
    /// 2 means that every digest_interval^2 there will be a level2-digest, and so on.
    /// Please ensure that maximum digest interval (i.e. digest_interval^digest_levels)
    /// is within `u32` limits. Otherwise you'll never see digests covering such intervals
    /// && maximal digests interval will be truncated to the last interval that fits
    /// `u32` limits.
    pub digest_levels: u32,
}

/// Abstraction over a substrate block.
#[derive(Debug, PartialEq, Eq, Clone, Encode, Decode)]
pub struct Block {
    /// The block header.
    pub header: Header,
    /// The accompanying extrinsics.
    pub extrinsics: Vec<Extrinsic>,
}

impl Block {
    /// Returns the hash of the block.
    pub fn block_hash(&self) -> BlockHash {
        self.header.block_hash()
    }
}

/// A proof that some set of key-value pairs are included in the storage trie. The proof contains
/// the storage values so that the partial storage backend can be reconstructed by a verifier that
/// does not already have access to the key-value pairs.
///
/// The proof consists of the set of serialized nodes in the storage trie accessed when looking up
/// the keys covered by the proof. Verifying the proof requires constructing the partial trie from
/// the serialized nodes and performing the key lookups.
#[derive(Debug, PartialEq, Eq, Clone, Encode, Decode)]
pub struct StorageProof {
    trie_nodes: Vec<Vec<u8>>,
}