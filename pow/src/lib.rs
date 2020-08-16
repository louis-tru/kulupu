// Copyright 2019-2020 Wei Tang.
// This file is part of Kulupu.

// Kulupu is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Kulupu is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Kulupu.  If not, see <http://www.gnu.org/licenses/>.

use std::sync::{Arc, Mutex};
use std::cell::RefCell;
use codec::{Encode, Decode};
use sp_core::{U256, H256, sr25519, crypto::Pair};
use sp_api::ProvideRuntimeApi;
use sp_runtime::generic::BlockId;
use sp_runtime::traits::{
	Block as BlockT, Header as HeaderT, UniqueSaturatedInto,
};
use sp_consensus_pow::{Seal as RawSeal, DifficultyApi};
use sc_consensus_pow::PowAlgorithm;
use sc_client_api::{blockchain::HeaderBackend, backend::AuxStore};
use kulupu_primitives::{Difficulty, AlgorithmApi};
use lru_cache::LruCache;
use rand::{SeedableRng, thread_rng, rngs::SmallRng};
use lazy_static::lazy_static;
use kulupu_randomx as randomx;
use log::*;

#[derive(Clone, PartialEq, Eq, Encode, Decode, Debug)]
pub struct SealV1 {
	pub difficulty: Difficulty,
	pub nonce: H256,
}

#[derive(Clone, PartialEq, Eq, Encode, Decode, Debug)]
pub struct SealV2 {
	pub difficulty: Difficulty,
	pub nonce: H256,
	pub signature: sr25519::Signature,
}

#[derive(Clone, PartialEq, Eq, Encode, Decode, Debug)]
pub struct Calculation {
	pub pre_hash: H256,
	pub difficulty: Difficulty,
	pub nonce: H256,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ComputeV1 {
	pub key_hash: H256,
	pub pre_hash: H256,
	pub difficulty: Difficulty,
	pub nonce: H256,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ComputeV2 {
	pub key_hash: H256,
	pub pre_hash: H256,
	pub difficulty: Difficulty,
	pub nonce: H256,
}

lazy_static! {
	static ref SHARED_CACHES: Arc<Mutex<LruCache<H256, Arc<randomx::FullCache>>>> =
		Arc::new(Mutex::new(LruCache::new(2)));
}
thread_local!(static MACHINES: RefCell<Option<(H256, randomx::FullVM)>> = RefCell::new(None));

impl ComputeV2 {
	pub fn sign(&self, pair: &sr25519::Pair) -> sr25519::Signature {
		let calculation = Calculation {
			difficulty: self.difficulty,
			pre_hash: self.pre_hash,
			nonce: self.nonce,
		};

		pair.sign(&calculation.encode()[..])
	}

	pub fn verify(
		&self,
		signature: &sr25519::Signature,
		public: &sr25519::Public,
	) -> bool {
		let calculation = Calculation {
			difficulty: self.difficulty,
			pre_hash: self.pre_hash,
			nonce: self.nonce,
		};

		sr25519::Pair::verify(
			signature,
			&calculation.encode()[..],
			public
		)
	}

	pub fn compute(self, signature: sr25519::Signature) -> (SealV2, H256) {
		MACHINES.with(|m| {
			let mut ms = m.borrow_mut();
			let calculation = Calculation {
				difficulty: self.difficulty,
				pre_hash: self.pre_hash,
				nonce: self.nonce,
			};

			let need_new_vm = ms.as_ref().map(|(mkey_hash, _)| {
				mkey_hash != &self.key_hash
			}).unwrap_or(true);

			if need_new_vm {
				let mut shared_caches = SHARED_CACHES.lock().expect("Mutex poisioned");

				if let Some(cache) = shared_caches.get_mut(&self.key_hash) {
					*ms = Some((self.key_hash, randomx::FullVM::new(cache.clone())));
				} else {
					info!("At block boundary, generating new RandomX cache with key hash {} ...",
						  self.key_hash);
					let cache = Arc::new(randomx::FullCache::new(&self.key_hash[..]));
					shared_caches.insert(self.key_hash, cache.clone());
					*ms = Some((self.key_hash, randomx::FullVM::new(cache)));
				}
			}

			let work = ms.as_mut()
				.map(|(mkey_hash, vm)| {
					assert_eq!(mkey_hash, &self.key_hash,
							   "Condition failed checking cached key_hash. This is a bug");
					vm.calculate(&(calculation, &signature).encode()[..])
				})
				.expect("Local MACHINES always set to Some above; qed");

			(SealV2 {
				nonce: self.nonce,
				difficulty: self.difficulty,
				signature,
			}, H256::from(work))
		})
	}
}

/// Checks whether the given hash is above difficulty.
fn is_valid_hash(hash: &H256, difficulty: Difficulty) -> bool {
	let num_hash = U256::from(&hash[..]);
	let (_, overflowed) = num_hash.overflowing_mul(difficulty);

	!overflowed
}

fn key_hash<B, C>(
	client: &C,
	parent: &BlockId<B>
) -> Result<H256, sc_consensus_pow::Error<B>> where
	B: BlockT<Hash=H256>,
	C: HeaderBackend<B>,
{
	const PERIOD: u64 = 4096; // ~2.8 days
	const OFFSET: u64 = 128;  // 2 hours

	let parent_header = client.header(parent.clone())
		.map_err(|e| sc_consensus_pow::Error::Environment(
			format!("Client execution error: {:?}", e)
		))?
		.ok_or(sc_consensus_pow::Error::Environment(
			"Parent header not found".to_string()
		))?;
	let parent_number = UniqueSaturatedInto::<u64>::unique_saturated_into(*parent_header.number());

	let mut key_number = parent_number.saturating_sub(parent_number % PERIOD);
	if parent_number.saturating_sub(key_number) < OFFSET {
		key_number = key_number.saturating_sub(PERIOD);
	}

	let mut current = parent_header;
	while UniqueSaturatedInto::<u64>::unique_saturated_into(*current.number()) != key_number {
		current = client.header(BlockId::Hash(*current.parent_hash()))
			.map_err(|e| sc_consensus_pow::Error::Environment(
				format!("Client execution error: {:?}", e)
			))?
			.ok_or(sc_consensus_pow::Error::Environment(
				format!("Block with hash {:?} not found", current.hash())
			))?;
	}

	Ok(current.hash())
}

pub struct RandomXAlgorithm<C> {
	client: Arc<C>,
	pair: Option<sr25519::Pair>,
}

impl<C> RandomXAlgorithm<C> {
	pub fn new(client: Arc<C>, pair: Option<sr25519::Pair>) -> Self {
		Self { client, pair }
	}
}

impl<C> Clone for RandomXAlgorithm<C> {
	fn clone(&self) -> Self {
		Self { client: self.client.clone(), pair: self.pair.clone() }
	}
}

impl<B: BlockT<Hash=H256>, C> PowAlgorithm<B> for RandomXAlgorithm<C> where
	C: HeaderBackend<B> + AuxStore + ProvideRuntimeApi<B>,
	C::Api: DifficultyApi<B, Difficulty> + AlgorithmApi<B>,
{
	type Difficulty = Difficulty;

	fn difficulty(&self, parent: H256) -> Result<Difficulty, sc_consensus_pow::Error<B>> {
		let difficulty = self.client.runtime_api().difficulty(&BlockId::Hash(parent))
			.map_err(|e| sc_consensus_pow::Error::Environment(
				format!("Fetching difficulty from runtime failed: {:?}", e)
			));

		difficulty
	}

	fn verify(
		&self,
		parent: &BlockId<B>,
		pre_hash: &H256,
		pre_digest: Option<&[u8]>,
		seal: &RawSeal,
		difficulty: Difficulty,
	) -> Result<bool, sc_consensus_pow::Error<B>> {
		assert_eq!(
			self.client.runtime_api().identifier(parent)
				.map_err(|e| sc_consensus_pow::Error::Environment(
					format!("Fetching identifier from runtime failed: {:?}", e))
				)?,
			kulupu_primitives::ALGORITHM_IDENTIFIER_V2
		);

		let key_hash = key_hash(self.client.as_ref(), parent)?;

		let seal = match SealV2::decode(&mut &seal[..]) {
			Ok(seal) => seal,
			Err(_) => return Ok(false),
		};

		let compute = ComputeV2 {
			key_hash,
			difficulty,
			pre_hash: *pre_hash,
			nonce: seal.nonce,
		};

		match pre_digest {
			Some(pre_digest) => {
				let author = match sr25519::Public::decode(&mut &pre_digest[..]) {
					Ok(author) => author,
					Err(_) => return Ok(false),
				};

				if !compute.verify(&seal.signature, &author) {
					return Ok(false)
				}
			},
			None => return Ok(false),
		}


		let (computed_seal, computed_work) = compute.compute(seal.signature.clone());

		if computed_seal != seal {
			return Ok(false)
		}

		if !is_valid_hash(&computed_work, difficulty) {
			return Ok(false)
		}

		Ok(true)
	}

	fn mine(
		&self,
		parent: &BlockId<B>,
		pre_hash: &H256,
		pre_digest: Option<&[u8]>,
		difficulty: Difficulty,
		round: u32,
	) -> Result<Option<RawSeal>, sc_consensus_pow::Error<B>> {
		if let Some(pair) = &self.pair {
			match pre_digest {
				Some(pre_digest) => {
					let author = match sr25519::Public::decode(&mut &pre_digest[..]) {
						Ok(author) => author,
						Err(_) => {
							warn!(target: "kulupu-pow", "Author key decoding failed, not mining.");
							return Ok(None)
						},
					};

					if author != pair.public() {
						warn!(target: "kulupu-pow", "Author key mismatch, not mining.");
						return Ok(None)
					}
				},
				None => {
					warn!(target: "kulupu-pow", "Author key does not exist, not mining.");
					return Ok(None)
				},
			}

			let mut rng = SmallRng::from_rng(&mut thread_rng())
				.map_err(|e| sc_consensus_pow::Error::Environment(
					format!("Initialize RNG failed for mining: {:?}", e)
				))?;
			let key_hash = key_hash(self.client.as_ref(), parent)?;

			for _ in 0..round {
				let nonce = H256::random_using(&mut rng);

				let compute = ComputeV2 {
					key_hash,
					difficulty,
					pre_hash: *pre_hash,
					nonce,
				};

				let signature = compute.sign(pair);
				let (seal, work) = compute.compute(signature);

				if is_valid_hash(&work, difficulty) {
					return Ok(Some(seal.encode()))
				}
			}

			Ok(None)
		} else {
			warn!(target: "kulupu-pow", "Author not set, not mining.");

			Ok(None)
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use sp_core::{H256, U256};

	#[test]
	fn randomx_len() {
		assert_eq!(randomx::HASH_SIZE, 32);
	}

	#[test]
	fn randomx_collision() {
		let mut compute = Compute {
			key_hash: H256::from([210, 164, 216, 149, 3, 68, 116, 1, 239, 110, 111, 48, 180, 102, 53, 180, 91, 84, 242, 90, 101, 12, 71, 70, 75, 83, 17, 249, 214, 253, 71, 89]),
			pre_hash: H256::default(),
			difficulty: U256::default(),
			nonce: H256::default(),
		};
		let hash1 = compute.clone().compute();
		U256::one().to_big_endian(&mut compute.nonce[..]);
		let hash2 = compute.compute();
		assert!(hash1 != hash2);
	}
}
