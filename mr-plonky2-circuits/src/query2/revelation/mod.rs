use anyhow::Result;
use recursion_framework::{
    framework::{
        RecursiveCircuits, RecursiveCircuitsVerifierGagdet, RecursiveCircuitsVerifierTarget,
    },
    serialization::{deserialize, serialize},
};
use serde::{Deserialize, Serialize};
use std::{array::from_fn as create_array, collections::BTreeMap};

use plonky2::{
    hash::poseidon::PoseidonHash,
    iop::{
        target::Target,
        witness::{PartialWitness, WitnessWrite},
    },
    plonk::{
        circuit_builder::CircuitBuilder,
        circuit_data::{CircuitData, VerifierCircuitData, VerifierOnlyCircuitData},
        config::Hasher,
        proof::{ProofWithPublicInputs, ProofWithPublicInputsTarget},
    },
};

use crate::{
    api::{default_config, deserialize_proof, serialize_proof, ProofWithVK, C, D, F},
    block::{
        Parameters as BlockDbParameters, PublicInputs as BlockDbPublicInputs, NUM_IVC_PUBLIC_INPUTS,
    },
    eth::left_pad32,
    query2::block,
    types::PACKED_MAPPING_KEY_LEN,
    utils::Packer,
};

pub use self::circuit::RevelationCircuit;
use self::circuit::RevelationWires;

pub mod circuit;
mod public_inputs;
pub use self::public_inputs::RevelationPublicInputs;
/// Wires containing the main logic wires of the RevelationCircuit,
/// the verifier wires to check a crate::block proof (block db) and
/// the verifier wires to check a proof from query2/block circuit set.
/// The two const parameters are:
/// - `BLOCK_DB_DEPTH` the depth of the block db merkle tree, fixed since we
///   we use a fixed sparse merkle tree.
/// - `L` the number of NFT IDs to reveal
#[derive(Serialize, Deserialize)]
pub struct Parameters<const BLOCK_DB_DEPTH: usize, const L: usize> {
    /// The regular wires for the logic of RevelationCircuit
    revelation: RevelationWires<L>,
    /// The wires to verify a proof in the query2/block circuit set
    query2_block: RecursiveCircuitsVerifierTarget<D>,
    /// The actual set of potential proofs VK that can be generated by query2/block
    query2_block_circuit_set: RecursiveCircuits<F, C, D>,
    /// The wires to verify a single regular proof by crate::block module
    #[serde(serialize_with = "serialize", deserialize_with = "deserialize")]
    block_db: ProofWithPublicInputsTarget<D>,
    /// The circuit data of the revelation circuit, required to generate and verify
    /// a revelation proof.
    #[serde(serialize_with = "serialize", deserialize_with = "deserialize")]
    circuit_data: CircuitData<F, C, D>,
}

/// Circuit inputs for the revelation step which contains the
/// raw witnesses and the proof to verify in circuit.
/// The proof is any of the proofs contained in the `query2/block/` module.
pub struct RevelationRecursiveInput<const L: usize> {
    /// values expected by the RevelationCircuit main logic
    logic_inputs: RevelationCircuit<L>,
    /// The actual proof generated by query2/block module, the top one
    query2_block_proof: ProofWithVK,
    /// The actual proof generated by the block db module, each time a new block
    /// is preprocessed
    block_db_proof: ProofWithPublicInputs<F, C, D>,
}

impl<const L: usize> RevelationRecursiveInput<L> {
    pub fn new(
        mapping_keys: Vec<Vec<u8>>,
        query_min_block: usize,
        query_max_block: usize,
        query2_block_proof: Vec<u8>,
        block_db_proof: Vec<u8>,
    ) -> Result<RevelationRecursiveInput<L>> {
        // sort mapping keys depending on the last limb, as it is the only limb currently considered
        // in the circuit
        let sorted_keys = mapping_keys
            .iter()
            .map(|key| {
                let packed = left_pad32(key).pack();
                (*packed.last().unwrap(), packed)
            })
            .collect::<BTreeMap<_, _>>();
        let mut sorted_keys_iter = sorted_keys.into_iter();
        let keys = create_array(|_i| {
            if let Some((_, packed)) = sorted_keys_iter.next() {
                create_array(|j| packed[j])
            } else {
                [0u32; PACKED_MAPPING_KEY_LEN]
            }
        });
        let num_entries = mapping_keys.len();
        assert!(
            num_entries <= L,
            "Number of entries {} should not exceed fixed parameter L {}",
            num_entries,
            L
        );
        let main_inputs = RevelationCircuit {
            packed_keys: keys,
            num_entries: num_entries as u8,
            query_min_block_number: query_min_block,
            query_max_block_number: query_max_block,
        };
        Ok(RevelationRecursiveInput {
            logic_inputs: main_inputs,
            query2_block_proof: ProofWithVK::deserialize(&query2_block_proof)?,
            block_db_proof: deserialize_proof(&block_db_proof)?,
        })
    }
}

const QUERY2_BLOCK_NUM_IO: usize = block::BlockPublicInputs::<Target>::total_len();
const BLOCK_DB_NUM_IO: usize = NUM_IVC_PUBLIC_INPUTS;

impl<const BLOCK_DB_DEPTH: usize, const L: usize> Parameters<BLOCK_DB_DEPTH, L> {
    /// Arguments are the circuit sets used to generate the query2/block proofs
    /// and the block db proof, and the verification key of the block db circuit
    pub fn build(
        query2_block_set: &RecursiveCircuits<F, C, D>,
        block_db_circuit_set: &RecursiveCircuits<F, C, D>,
        block_db_verifier_data: &VerifierOnlyCircuitData<C, D>,
    ) -> Self
    where
        [(); <PoseidonHash as Hasher<F>>::HASH_SIZE]:,
    {
        let mut b = CircuitBuilder::new(default_config());
        // instantiate the wires to verify a query2/block proof which can be in a circuit set
        let query2_block_verifier_gadget =
            RecursiveCircuitsVerifierGagdet::<F, C, D, QUERY2_BLOCK_NUM_IO>::new(
                default_config(),
                query2_block_set,
            );
        let query2_block_verifier_wires =
            query2_block_verifier_gadget.verify_proof_in_circuit_set(&mut b);
        let query2_block_pi = block::BlockPublicInputs::<Target>::from(
            query2_block_verifier_wires.get_public_input_targets::<F, QUERY2_BLOCK_NUM_IO>(),
        );

        assert_eq!(query2_block_pi.inputs.len(), QUERY2_BLOCK_NUM_IO);
        // instantiate the wires to verify a block db proof
        let block_db_verifier_gadget =
            RecursiveCircuitsVerifierGagdet::<F, C, D, BLOCK_DB_NUM_IO>::new(
                default_config(),
                block_db_circuit_set,
            );
        // we enforce that the db proof is generated with the IVC circuit, not the dummy one
        let block_db_wires = block_db_verifier_gadget
            .verify_proof_fixed_circuit_in_circuit_set(&mut b, block_db_verifier_data);
        let block_db_pi = BlockDbPublicInputs::from(
            BlockDbParameters::<BLOCK_DB_DEPTH>::block_tree_public_input_targets(&block_db_wires),
        );

        let wires =
            RevelationCircuit::build::<BLOCK_DB_DEPTH>(&mut b, block_db_pi, query2_block_pi);

        let circuit_data = b.build::<C>();
        Self {
            revelation: wires,
            query2_block: query2_block_verifier_wires,
            query2_block_circuit_set: query2_block_set.clone(),
            block_db: block_db_wires,
            circuit_data,
        }
    }
    fn generate_proof_internal(
        &self,
        inputs: RevelationRecursiveInput<L>,
    ) -> Result<ProofWithPublicInputs<F, C, D>>
    where
        [(); <PoseidonHash as Hasher<F>>::HASH_SIZE]:,
    {
        let mut pw = PartialWitness::new();
        // assigns the block db proof, simple verifier target
        pw.set_proof_with_pis_target(&self.block_db, &inputs.block_db_proof);
        // assigns the query2/block proof, recursive verifier target
        let (proof, vd) = inputs.query2_block_proof.into();
        self.query2_block
            .set_target(&mut pw, &self.query2_block_circuit_set, &proof, &vd)?;
        // assigns the regular wires
        inputs.logic_inputs.assign(&mut pw, &self.revelation);
        let proof = self.circuit_data.prove(pw)?;
        Ok(proof)
    }

    pub fn generate_proof(&self, inputs: RevelationRecursiveInput<L>) -> Result<Vec<u8>> {
        let proof = self.generate_proof_internal(inputs)?;
        serialize_proof(&proof)
    }
    pub fn circuit_data(&self) -> &CircuitData<F, C, D> {
        &self.circuit_data
    }
    pub fn verifier_data(&self) -> VerifierCircuitData<F, C, D> {
        self.circuit_data.verifier_data()
    }
    pub fn verify_proof(&self, proof: Vec<u8>) -> Result<()> {
        let proof = deserialize_proof(&proof)?;
        self.circuit_data.verify(proof)
    }
}

#[cfg(test)]
mod test {
    use std::iter::once;

    use crate::{
        api::{serialize_proof, ProofWithVK},
        block::empty_merkle_root,
        eth::left_pad,
        keccak::PACKED_HASH_LEN,
        query2::revelation::{RevelationRecursiveInput, QUERY2_BLOCK_NUM_IO},
        types::MAPPING_KEY_LEN,
        utils::{Packer, ToFields},
    };
    use anyhow::Result;
    use ethers::types::Address;
    use itertools::Itertools;
    use plonky2::{
        field::{
            goldilocks_field::GoldilocksField,
            types::{Field, PrimeField64, Sample},
        },
        hash::hash_types::{HashOut, NUM_HASH_OUT_ELTS},
    };
    use rand::{thread_rng, Rng};
    use recursion_framework::framework_testing::TestingRecursiveCircuits;
    use serial_test::serial;

    use super::*;

    use crate::{
        api::{C, D, F},
        eth::left_pad32,
        group_hashing,
        query2::block::BlockPublicInputs,
    };

    #[test]
    #[serial]
    fn test_revelation_api() -> Result<()> {
        // Generate a fake query2/block circuit set
        let query2_testing_framework =
            TestingRecursiveCircuits::<F, C, D, QUERY2_BLOCK_NUM_IO>::default();
        let query2_block_circuit_set = query2_testing_framework.get_recursive_circuit_set();

        // Generate a fake block/ verification key
        let block_db_testing_framework =
            TestingRecursiveCircuits::<F, C, D, BLOCK_DB_NUM_IO>::default();
        let block_db_circuit_set = block_db_testing_framework.get_recursive_circuit_set();

        let block_db_vk = block_db_testing_framework.verifier_data_for_input_proofs::<1>()[0];
        // Build the params
        const L: usize = 2;
        const BLOCK_DB_DEPTH: usize = 2;
        let params = super::Parameters::<BLOCK_DB_DEPTH, L>::build(
            query2_block_circuit_set,
            block_db_circuit_set,
            block_db_vk,
        );

        // Generate a fake block db proof
        let init_root = empty_merkle_root::<GoldilocksField, 2, BLOCK_DB_DEPTH>();
        let last_root = HashOut {
            elements: F::rand_vec(NUM_HASH_OUT_ELTS).try_into().unwrap(),
        };
        let init_block_number = F::from_canonical_u32(thread_rng().gen::<u32>());
        let db_range = 555;
        let last_block_number = init_block_number + F::from_canonical_usize(db_range);
        let last_block_hash = F::rand_vec(PACKED_HASH_LEN);

        let block_db_inputs: [F; BLOCK_DB_NUM_IO] = BlockDbPublicInputs::from_parts(
            &init_root.elements,
            &last_root.elements,
            init_block_number,
            last_block_number,
            &last_block_hash.try_into().unwrap(),
        )
        .into_iter()
        .chain(once(F::ONE))
        .collect_vec()
        .try_into()
        .unwrap();
        let block_db_pi = BlockDbPublicInputs::<GoldilocksField>::from(&block_db_inputs);
        let block_db_proof =
            &block_db_testing_framework.generate_input_proofs::<1>([block_db_inputs.clone()])?[0];

        // Generate a fake query2/block proof, taking some inputs from the block db
        // block range asked is just one block less than latest block in db
        let query_max_number = block_db_pi.block_number_data() - F::ONE;
        let query_range = F::from_canonical_usize(10);
        let query_min_number = query_max_number - query_range + F::ONE;
        let query_root = HashOut {
            elements: block_db_pi.root_data().try_into().unwrap(),
        };
        let smc_address = Address::random();
        let user_address = Address::random();
        let mapping_slot = F::rand();
        let length_slot = F::rand();
        let mapping_keys = (0..L)
            .map(|_| left_pad::<MAPPING_KEY_LEN>(&[thread_rng().gen::<u8>()]))
            .collect::<Vec<_>>();
        let packed_field_mks = mapping_keys
            .iter()
            .map(|x| x.pack().to_fields())
            .collect::<Vec<_>>();
        let digests = packed_field_mks
            .iter()
            .map(|i| group_hashing::map_to_curve_point(i))
            .collect::<Vec<_>>();
        let single_digest = group_hashing::add_curve_point(&digests);
        let pis = BlockPublicInputs::from_parts(
            query_max_number,
            query_range,
            query_root,
            &smc_address
                .as_fixed_bytes()
                .pack()
                .to_fields()
                .try_into()
                .unwrap(),
            &left_pad32(user_address.as_fixed_bytes())
                .pack()
                .to_fields()
                .try_into()
                .unwrap(),
            mapping_slot,
            length_slot,
            single_digest.to_weierstrass(),
        );
        let query2_block_proof = query2_testing_framework
            .generate_input_proofs([pis])
            .unwrap();
        let query2_block_vd = query2_testing_framework.verifier_data_for_input_proofs::<1>();

        let q2_proof_buff = ProofWithVK {
            proof: query2_block_proof[0].clone(),
            vk: query2_block_vd[0].clone(),
        }
        .serialize()?;
        let block_db_buff = serialize_proof(block_db_proof)?;
        let revelation_inputs = RevelationRecursiveInput::<L>::new(
            mapping_keys.into_iter().map(|x| x.to_vec()).collect(),
            query_min_number.to_canonical_u64() as usize,
            query_max_number.to_canonical_u64() as usize,
            q2_proof_buff,
            block_db_buff,
        )?;
        println!("generating revelation proof");
        let proof = params.generate_proof(revelation_inputs)?;
        params.verify_proof(proof)?;
        Ok(())
    }
}
