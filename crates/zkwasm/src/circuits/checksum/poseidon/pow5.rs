use std::convert::TryInto;
use std::iter;

use halo2_proofs::arithmetic::FieldExt;
use halo2_proofs::circuit::AssignedCell;
use halo2_proofs::circuit::Chip;
use halo2_proofs::circuit::Layouter;
use halo2_proofs::circuit::Region;
use halo2_proofs::pairing::group::ff::Field;
use halo2_proofs::plonk::Advice;
use halo2_proofs::plonk::Any;
use halo2_proofs::plonk::Column;
use halo2_proofs::plonk::ConstraintSystem;
use halo2_proofs::plonk::Error;
use halo2_proofs::plonk::Fixed;
use halo2_proofs::poly::Rotation;

use super::primitives::Absorbing;
use super::primitives::Domain;
use super::primitives::Mds;
use super::primitives::Spec;
use super::primitives::Squeezing;
use super::primitives::State;
use super::PaddedWord;
use super::PoseidonInstructions;
use super::PoseidonSpongeInstructions;

/// Configuration for a [`Pow5Chip`].
#[derive(Clone, Debug)]
pub struct Pow5Config<F: FieldExt, const WIDTH: usize, const RATE: usize> {
    pub(crate) state: [Column<Advice>; WIDTH],
    pub(crate) state_rc_a: [Column<Advice>; WIDTH],
    pub(crate) state_rc_a_sqr: [Column<Advice>; WIDTH],
    partial_sbox: Column<Advice>,
    mid_0_helper: Column<Advice>,
    mid_0_helper_sqr: Column<Advice>,
    cur_0_rc_a0: Column<Advice>,
    cur_0_rc_a0_sqr: Column<Advice>,
    rc_a: [Column<Fixed>; WIDTH],
    rc_b: [Column<Fixed>; WIDTH],
    s_full: Column<Fixed>,
    s_partial: Column<Fixed>,
    s_pad_and_add: Column<Fixed>,

    half_full_rounds: usize,
    half_partial_rounds: usize,
    alpha: [u64; 4],
    round_constants: Vec<[F; WIDTH]>,
    m_reg: Mds<F, WIDTH>,
}

/// A Poseidon chip using an $x^5$ S-Box.
///
/// The chip is implemented using a single round per row for full rounds, and two rounds
/// per row for partial rounds.
#[derive(Debug)]
pub struct Pow5Chip<F: FieldExt, const WIDTH: usize, const RATE: usize> {
    config: Pow5Config<F, WIDTH, RATE>,
}

impl<F: FieldExt, const WIDTH: usize, const RATE: usize> Pow5Chip<F, WIDTH, RATE> {
    /// Configures this chip for use in a circuit.
    ///
    /// # Side-effects
    ///
    /// All columns in `state` will be equality-enabled.
    //
    // TODO: Does the rate need to be hard-coded here, or only the width? It probably
    // needs to be known wherever we implement the hashing gadget, but it isn't strictly
    // necessary for the permutation.
    pub fn configure<S: Spec<F, WIDTH, RATE>>(
        meta: &mut ConstraintSystem<F>,
        state: [Column<Advice>; WIDTH],
        state_rc_a: [Column<Advice>; WIDTH],
        state_rc_a_sqr: [Column<Advice>; WIDTH],
        partial_sbox: Column<Advice>,
        mid_0_helper: Column<Advice>,
        mid_0_helper_sqr: Column<Advice>,
        cur_0_rc_a0: Column<Advice>,
        cur_0_rc_a0_sqr: Column<Advice>,
        rc_a: [Column<Fixed>; WIDTH],
        rc_b: [Column<Fixed>; WIDTH],
    ) -> Pow5Config<F, WIDTH, RATE> {
        assert_eq!(RATE, WIDTH - 1);
        // Generate constants for the Poseidon permutation.
        // This gadget requires R_F and R_P to be even.
        assert!(S::full_rounds() & 1 == 0);
        assert!(S::partial_rounds() & 1 == 0);
        let half_full_rounds = S::full_rounds() / 2;
        let half_partial_rounds = S::partial_rounds() / 2;
        let (round_constants, m_reg, m_inv) = S::constants();

        // This allows state words to be initialized (by constraining them equal to fixed
        // values), and used in a permutation from an arbitrary region. rc_a is used in
        // every permutation round, while rc_b is empty in the initial and final full
        // rounds, so we use rc_b as "scratch space" for fixed values (enabling potential
        // layouter optimisations).
        for column in iter::empty()
            .chain(state.iter().cloned().map(Column::<Any>::from))
            .chain(rc_b.iter().cloned().map(Column::<Any>::from))
        {
            meta.enable_equality(column);
        }

        let s_full = meta.fixed_column();
        let s_partial = meta.fixed_column();
        let s_pad_and_add = meta.fixed_column();

        let alpha = [5, 0, 0, 0];

        meta.create_gate("full round state_rc_a", |meta| {
            let s_full = meta.query_fixed(s_full, Rotation::cur());

            (0..WIDTH)
                .map(|idx| {
                    let state_cur = meta.query_advice(state[idx], Rotation::cur());
                    let rc_a = meta.query_fixed(rc_a[idx], Rotation::cur());
                    let state_rc_a_cur = meta.query_advice(state_rc_a[idx], Rotation::cur());
                    (state_cur + rc_a - state_rc_a_cur) * s_full.clone()
                })
                .into_iter()
                .collect::<Vec<_>>()
        });

        meta.create_gate("full round state_rc_a_sqr", |meta| {
            let s_full = meta.query_fixed(s_full, Rotation::cur());

            (0..WIDTH)
                .map(|idx| {
                    let state_rc_a_sqr_cur =
                        meta.query_advice(state_rc_a_sqr[idx], Rotation::cur());
                    let state_rc_a_cur = meta.query_advice(state_rc_a[idx], Rotation::cur());
                    (state_rc_a_cur.clone() * state_rc_a_cur - state_rc_a_sqr_cur) * s_full.clone()
                })
                .into_iter()
                .collect::<Vec<_>>()
        });

        meta.create_gate("full round", |meta| {
            let s_full = meta.query_fixed(s_full, Rotation::cur());

            (0..WIDTH)
                .map(|next_idx| {
                    let state_next = meta.query_advice(state[next_idx], Rotation::next());
                    let expr = (0..WIDTH)
                        .map(|idx| {
                            let state_rc_a_sqr_cur =
                                meta.query_advice(state_rc_a_sqr[idx], Rotation::cur());
                            let state_rc_a_cur =
                                meta.query_advice(state_rc_a[idx], Rotation::cur());
                            state_rc_a_cur
                                * state_rc_a_sqr_cur.clone()
                                * state_rc_a_sqr_cur
                                * m_reg[next_idx][idx]
                        })
                        .reduce(|acc, term| acc + term)
                        .expect("WIDTH > 0");
                    s_full.clone() * (expr - state_next)
                })
                .collect::<Vec<_>>()
        });

        meta.create_gate("cur0_rc_a0", |meta| {
            let cur_0 = meta.query_advice(state[0], Rotation::cur());
            let rc_a0 = meta.query_fixed(rc_a[0], Rotation::cur());
            let cur0_rc_a0 = meta.query_advice(cur_0_rc_a0, Rotation::cur());
            let cur0_rc_a0_sqr = meta.query_advice(cur_0_rc_a0_sqr, Rotation::cur());
            let s_partial = meta.query_fixed(s_partial, Rotation::cur());

            vec![
                (cur0_rc_a0.clone() - cur_0 - rc_a0) * s_partial.clone(),
                (cur0_rc_a0_sqr - cur0_rc_a0.clone() * cur0_rc_a0) * s_partial,
            ]
        });

        meta.create_gate("mid_0_helper", |meta| {
            let mid_0 = meta.query_advice(partial_sbox, Rotation::cur());
            let rc_b0 = meta.query_fixed(rc_b[0], Rotation::cur());
            let mid_0_helper_curr = meta.query_advice(mid_0_helper, Rotation::cur());
            let mid_0_helper_sqr_curr = meta.query_advice(mid_0_helper_sqr, Rotation::cur());
            let s_partial = meta.query_fixed(s_partial, Rotation::cur());

            use halo2_proofs::plonk::VirtualCells;
            let mid = |idx: usize, meta: &mut VirtualCells<F>| {
                let mid = mid_0.clone() * m_reg[idx][0];
                (1..WIDTH).fold(mid, |acc, cur_idx| {
                    let cur = meta.query_advice(state[cur_idx], Rotation::cur());
                    let rc_a = meta.query_fixed(rc_a[cur_idx], Rotation::cur());
                    acc + (cur + rc_a) * m_reg[idx][cur_idx]
                })
            };

            vec![
                (mid_0_helper_curr.clone() - mid(0, meta) - rc_b0) * s_partial.clone(),
                (mid_0_helper_sqr_curr - mid_0_helper_curr.clone() * mid_0_helper_curr) * s_partial,
            ]
        });

        meta.create_gate("partial rounds", |meta| {
            let mid_0 = meta.query_advice(partial_sbox, Rotation::cur());
            let mid_0_helper_curr = meta.query_advice(mid_0_helper, Rotation::cur());
            let mid_0_helper_sqr_curr = meta.query_advice(mid_0_helper_sqr, Rotation::cur());
            let cur_0_rc_a0_curr = meta.query_advice(cur_0_rc_a0, Rotation::cur());
            let cur_0_rc_a0_sqr_curr = meta.query_advice(cur_0_rc_a0_sqr, Rotation::cur());

            let s_partial = meta.query_fixed(s_partial, Rotation::cur());

            use halo2_proofs::plonk::VirtualCells;
            let mid = |idx: usize, meta: &mut VirtualCells<F>| {
                let mid = mid_0.clone() * m_reg[idx][0];
                (1..WIDTH).fold(mid, |acc, cur_idx| {
                    let cur = meta.query_advice(state[cur_idx], Rotation::cur());
                    let rc_a = meta.query_fixed(rc_a[cur_idx], Rotation::cur());
                    acc + (cur + rc_a) * m_reg[idx][cur_idx]
                })
            };

            let next = |idx: usize, meta: &mut VirtualCells<F>| {
                (0..WIDTH)
                    .map(|next_idx| {
                        let next = meta.query_advice(state[next_idx], Rotation::next());
                        next * m_inv[idx][next_idx]
                    })
                    .reduce(|acc, next| acc + next)
                    .expect("WIDTH > 0")
            };

            let partial_round_linear = |idx: usize, meta: &mut VirtualCells<F>| {
                let rc_b = meta.query_fixed(rc_b[idx], Rotation::cur());
                mid(idx, meta) + rc_b - next(idx, meta)
            };

            std::iter::empty()
                // state[0] round a
                .chain(Some(
                    cur_0_rc_a0_sqr_curr.clone() * cur_0_rc_a0_sqr_curr * cur_0_rc_a0_curr
                        - mid_0.clone(),
                ))
                // state[0] round b
                .chain(Some(
                    mid_0_helper_sqr_curr.clone() * mid_0_helper_sqr_curr * mid_0_helper_curr
                        - next(0, meta),
                ))
                .chain((1..WIDTH).map(|idx| partial_round_linear(idx, meta)))
                .map(|x| s_partial.clone() * x)
                .collect::<Vec<_>>()
        });

        meta.create_gate("pad-and-add", |meta| {
            let initial_state_rate = meta.query_advice(state[RATE], Rotation::prev());
            let output_state_rate = meta.query_advice(state[RATE], Rotation::next());

            let s_pad_and_add = meta.query_fixed(s_pad_and_add, Rotation::cur());

            let pad_and_add = |idx: usize| {
                let initial_state = meta.query_advice(state[idx], Rotation::prev());
                let input = meta.query_advice(state[idx], Rotation::cur());
                let output_state = meta.query_advice(state[idx], Rotation::next());

                // We pad the input by storing the required padding in fixed columns and
                // then constraining the corresponding input columns to be equal to it.
                initial_state + input - output_state
            };
            (0..RATE)
                .map(pad_and_add)
                // The capacity element is never altered by the input.
                .chain(Some(initial_state_rate - output_state_rate))
                .map(|x| s_pad_and_add.clone() * x)
                .collect::<Vec<_>>()
        });

        Pow5Config {
            state,
            state_rc_a,
            state_rc_a_sqr,
            partial_sbox,
            mid_0_helper,
            mid_0_helper_sqr,
            cur_0_rc_a0,
            cur_0_rc_a0_sqr,
            rc_a,
            rc_b,
            s_full,
            s_partial,
            s_pad_and_add,
            half_full_rounds,
            half_partial_rounds,
            alpha,
            round_constants,
            m_reg,
        }
    }

    /// Construct a [`Pow5Chip`].
    pub fn construct(config: Pow5Config<F, WIDTH, RATE>) -> Self {
        Pow5Chip { config }
    }
}

impl<F: FieldExt, const WIDTH: usize, const RATE: usize> Chip<F> for Pow5Chip<F, WIDTH, RATE> {
    type Config = Pow5Config<F, WIDTH, RATE>;
    type Loaded = ();

    fn config(&self) -> &Self::Config {
        &self.config
    }

    fn loaded(&self) -> &Self::Loaded {
        &()
    }
}

impl<F: FieldExt, S: Spec<F, WIDTH, RATE>, const WIDTH: usize, const RATE: usize>
    PoseidonInstructions<F, S, WIDTH, RATE> for Pow5Chip<F, WIDTH, RATE>
{
    type Word = StateWord<F>;

    fn permute(
        &self,
        layouter: &mut impl Layouter<F>,
        initial_state: &State<Self::Word, WIDTH>,
    ) -> Result<State<Self::Word, WIDTH>, Error> {
        let config = self.config();

        layouter.assign_region(
            || "permute state",
            |mut region| {
                // Load the initial state into this region.
                let state = Pow5State::load(&mut region, config, initial_state)?;

                let state = (0..config.half_full_rounds).fold(Ok(state), |res, r| {
                    res.and_then(|state| state.full_round(&mut region, config, r, r))
                })?;

                let state = (0..config.half_partial_rounds).fold(Ok(state), |res, r| {
                    res.and_then(|state| {
                        state.partial_round(
                            &mut region,
                            config,
                            config.half_full_rounds + 2 * r,
                            config.half_full_rounds + r,
                        )
                    })
                })?;

                let state = (0..config.half_full_rounds).fold(Ok(state), |res, r| {
                    res.and_then(|state| {
                        state.full_round(
                            &mut region,
                            config,
                            config.half_full_rounds + 2 * config.half_partial_rounds + r,
                            config.half_full_rounds + config.half_partial_rounds + r,
                        )
                    })
                })?;

                Ok(state.0)
            },
        )
    }
}

impl<
        F: FieldExt,
        S: Spec<F, WIDTH, RATE>,
        D: Domain<F, RATE>,
        const WIDTH: usize,
        const RATE: usize,
    > PoseidonSpongeInstructions<F, S, D, WIDTH, RATE> for Pow5Chip<F, WIDTH, RATE>
{
    fn initial_state(
        &self,
        layouter: &mut impl Layouter<F>,
    ) -> Result<State<Self::Word, WIDTH>, Error> {
        let config = self.config();
        let state = layouter.assign_region(
            || format!("initial state for domain {}", D::name()),
            |mut region| {
                let mut state = Vec::with_capacity(WIDTH);
                let mut load_state_word = |i: usize, value: F| -> Result<_, Error> {
                    let var = region.assign_advice_from_constant(
                        || format!("state_{}", i),
                        config.state[i],
                        0,
                        value,
                    )?;
                    state.push(StateWord(var));

                    Ok(())
                };

                for i in 0..RATE {
                    load_state_word(i, F::zero())?;
                }
                load_state_word(RATE, D::initial_capacity_element())?;

                Ok(state)
            },
        )?;

        Ok(state.try_into().unwrap())
    }

    fn add_input(
        &self,
        layouter: &mut impl Layouter<F>,
        initial_state: &State<Self::Word, WIDTH>,
        input: &Absorbing<PaddedWord<F>, RATE>,
    ) -> Result<State<Self::Word, WIDTH>, Error> {
        let config = self.config();
        layouter.assign_region(
            || format!("add input for domain {}", D::name()),
            |mut region| {
                region.assign_fixed(
                    || "s_pad_and_add",
                    config.s_pad_and_add,
                    1,
                    || Ok(F::one()),
                )?;

                // Load the initial state into this region.
                let load_state_word = |i: usize| {
                    initial_state[i]
                        .0
                        .copy_advice(
                            || format!("load state_{}", i),
                            &mut region,
                            config.state[i],
                            0,
                        )
                        .map(StateWord)
                };
                let initial_state: Result<Vec<_>, Error> =
                    (0..WIDTH).map(load_state_word).collect();
                let initial_state = initial_state?;

                // Load the input into this region.
                let load_input_word = |i: usize| {
                    let constraint_var = match input.0[i].clone() {
                        Some(PaddedWord::Message(word)) => (word.value().copied(), word),
                        Some(PaddedWord::Padding(padding_value)) => (
                            Some(padding_value),
                            region.assign_fixed(
                                || format!("load pad_{}", i),
                                config.rc_b[i],
                                1,
                                || Ok(padding_value),
                            )?,
                        ),
                        _ => panic!("Input is not padded"),
                    };

                    let cell = region.assign_advice(
                        || format!("load input_{}", i),
                        config.state[i],
                        1,
                        || Ok(constraint_var.0.unwrap()),
                    )?;
                    region.constrain_equal(cell.cell(), constraint_var.1.cell())?;

                    Ok(StateWord(cell))
                };
                let input: Vec<_> = (0..RATE)
                    .map(load_input_word)
                    .collect::<Result<_, Error>>()?;

                // Constrain the output.
                let constrain_output_word = |i: usize| {
                    let value = initial_state[i].0.value().copied().unwrap_or(F::zero())
                        + input
                            .get(i)
                            .map(|word| word.0.value().cloned().unwrap_or(F::zero()))
                            // The capacity element is never altered by the input.
                            .unwrap_or(F::zero());
                    region
                        .assign_advice(
                            || format!("load output_{}", i),
                            config.state[i],
                            2,
                            || Ok(value),
                        )
                        .map(StateWord)
                };

                let output: Result<Vec<_>, Error> = (0..WIDTH).map(constrain_output_word).collect();
                output.map(|output| output.try_into().unwrap())
            },
        )
    }

    fn get_output(state: &State<Self::Word, WIDTH>) -> Squeezing<Self::Word, RATE> {
        Squeezing(
            state[..RATE]
                .iter()
                .map(|word| Some(word.clone()))
                .collect::<Vec<_>>()
                .try_into()
                .unwrap(),
        )
    }
}

/// A word in the Poseidon state.
#[derive(Clone, Debug)]
pub struct StateWord<F: Field>(pub(crate) AssignedCell<F, F>);

impl<F: Field> From<StateWord<F>> for AssignedCell<F, F> {
    fn from(state_word: StateWord<F>) -> AssignedCell<F, F> {
        state_word.0
    }
}

impl<F: Field> From<AssignedCell<F, F>> for StateWord<F> {
    fn from(cell_value: AssignedCell<F, F>) -> StateWord<F> {
        StateWord(cell_value)
    }
}

#[derive(Debug)]
struct Pow5State<F: FieldExt, const WIDTH: usize>([StateWord<F>; WIDTH]);

impl<F: FieldExt, const WIDTH: usize> Pow5State<F, WIDTH> {
    fn full_round<const RATE: usize>(
        self,
        region: &mut Region<F>,
        config: &Pow5Config<F, WIDTH, RATE>,
        round: usize,
        offset: usize,
    ) -> Result<Self, Error> {
        Self::round(region, config, round, offset, config.s_full, |region| {
            let q = self.0.iter().enumerate().map(|(idx, word)| {
                word.0
                    .value()
                    .map(|v| *v + config.round_constants[round][idx])
            });

            let state_rc_a: Option<Vec<F>> = q.collect();
            for i in 0..WIDTH {
                region.assign_advice(
                    || format!("round_{} state_rc_a", round),
                    config.state_rc_a[i],
                    offset,
                    || Ok(state_rc_a.as_ref().unwrap()[i]),
                )?;

                region.assign_advice(
                    || format!("round_{} state_rc_a_sqr", round),
                    config.state_rc_a_sqr[i],
                    offset,
                    || Ok(state_rc_a.as_ref().unwrap()[i].square()),
                )?;
            }

            let r: Option<Vec<F>> =
                state_rc_a.map(|q| q.into_iter().map(|q| q.pow(&config.alpha)).collect());
            let m = &config.m_reg;
            let state = m.iter().map(|m_i| {
                r.as_ref()
                    .map(|r| {
                        r.iter()
                            .enumerate()
                            .fold(F::zero(), |acc, (j, r_j)| acc + m_i[j] * r_j)
                    })
                    .unwrap_or_default()
            });

            Ok((round + 1, state.collect::<Vec<_>>().try_into().unwrap()))
        })
    }

    fn partial_round<const RATE: usize>(
        self,
        region: &mut Region<F>,
        config: &Pow5Config<F, WIDTH, RATE>,
        round: usize,
        offset: usize,
    ) -> Result<Self, Error> {
        Self::round(region, config, round, offset, config.s_partial, |region| {
            let m = &config.m_reg;
            let p: Option<Vec<_>> = self.0.iter().map(|word| word.0.value().cloned()).collect();

            let r: Option<Vec<_>> = p.map(|p| {
                let r_0 = (p[0] + config.round_constants[round][0]).pow(&config.alpha);
                let r_i = p[1..]
                    .iter()
                    .enumerate()
                    .map(|(i, p_i)| *p_i + config.round_constants[round][i + 1]);
                std::iter::empty().chain(Some(r_0)).chain(r_i).collect()
            });

            region.assign_advice(
                || format!("round_{} partial_sbox", round),
                config.partial_sbox,
                offset,
                || Ok(r.as_ref().map(|r| r[0]).unwrap()),
            )?;

            let p_mid: Option<Vec<_>> = m
                .iter()
                .map(|m_i| {
                    r.as_ref().map(|r| {
                        m_i.iter()
                            .zip(r.iter())
                            .fold(F::zero(), |acc, (m_ij, r_j)| acc + *m_ij * r_j)
                    })
                })
                .collect();

            // Load the second round constants.
            let mut load_round_constant = |i: usize| {
                region.assign_fixed(
                    || format!("round_{} rc_{}", round + 1, i),
                    config.rc_b[i],
                    offset,
                    || Ok(config.round_constants[round + 1][i]),
                )
            };
            for i in 0..WIDTH {
                load_round_constant(i)?;
            }

            let mid_0_helper_curr = p_mid
                .as_ref()
                .and_then(|x| Some(x[0] + config.round_constants[round + 1][0]));

            region.assign_advice(
                || format!("round_{} mid_0_helper_curr", round),
                config.mid_0_helper,
                offset,
                || Ok(mid_0_helper_curr.unwrap()),
            )?;

            region.assign_advice(
                || format!("round_{} mid_0_helper_sqr_curr", round),
                config.mid_0_helper_sqr,
                offset,
                || Ok(mid_0_helper_curr.unwrap().square()),
            )?;

            let cur_0_rc_a0 = self.0[0]
                .0
                .value()
                .cloned()
                .and_then(|x| Some(config.round_constants[round][0] + x));

            region.assign_advice(
                || format!("round_{} mid_0_helper_curr", round),
                config.cur_0_rc_a0,
                offset,
                || Ok(cur_0_rc_a0.unwrap()),
            )?;

            region.assign_advice(
                || format!("round_{} mid_0_helper_sqr_curr", round),
                config.cur_0_rc_a0_sqr,
                offset,
                || Ok(cur_0_rc_a0.unwrap().square()),
            )?;

            let r_mid: Option<Vec<_>> = p_mid.map(|p| {
                let r_0 = (p[0] + config.round_constants[round + 1][0]).pow(&config.alpha);
                let r_i = p[1..]
                    .iter()
                    .enumerate()
                    .map(|(i, p_i)| *p_i + config.round_constants[round + 1][i + 1]);
                std::iter::empty().chain(Some(r_0)).chain(r_i).collect()
            });

            let state: Vec<_> = m
                .iter()
                .map(|m_i| {
                    r_mid
                        .as_ref()
                        .map(|r| {
                            m_i.iter()
                                .zip(r.iter())
                                .fold(F::zero(), |acc, (m_ij, r_j)| acc + *m_ij * r_j)
                        })
                        .unwrap_or_default()
                })
                .collect();

            Ok((round + 2, state.try_into().unwrap()))
        })
    }

    fn load<const RATE: usize>(
        region: &mut Region<F>,
        config: &Pow5Config<F, WIDTH, RATE>,
        initial_state: &State<StateWord<F>, WIDTH>,
    ) -> Result<Self, Error> {
        let load_state_word = |i: usize| {
            initial_state[i]
                .0
                .copy_advice(|| format!("load state_{}", i), region, config.state[i], 0)
                .map(StateWord)
        };

        let state: Result<Vec<_>, _> = (0..WIDTH).map(load_state_word).collect();
        state.map(|state| Pow5State(state.try_into().unwrap()))
    }

    fn round<const RATE: usize>(
        region: &mut Region<F>,
        config: &Pow5Config<F, WIDTH, RATE>,
        round: usize,
        offset: usize,
        round_gate: Column<Fixed>,
        round_fn: impl FnOnce(&mut Region<F>) -> Result<(usize, [F; WIDTH]), Error>,
    ) -> Result<Self, Error> {
        // Enable the required gate.
        region.assign_fixed(|| "round", round_gate, offset, || Ok(F::one()))?;

        // Load the round constants.
        let mut load_round_constant = |i: usize| {
            region.assign_fixed(
                || format!("round_{} rc_{}", round, i),
                config.rc_a[i],
                offset,
                || Ok(config.round_constants[round][i]),
            )
        };
        for i in 0..WIDTH {
            load_round_constant(i)?;
        }

        // Compute the next round's state.
        let (next_round, next_state) = round_fn(region)?;

        let next_state_word = |i: usize| {
            let value = next_state[i];
            let var = region.assign_advice(
                || format!("round_{} state_{}", next_round, i),
                config.state[i],
                offset + 1,
                || Ok(value),
            )?;
            Ok(StateWord(var))
        };

        let next_state: Result<Vec<_>, _> = (0..WIDTH).map(next_state_word).collect();
        next_state.map(|next_state| Pow5State(next_state.try_into().unwrap()))
    }
}

#[cfg(test)]
mod tests {
    use halo2_proofs::arithmetic::Field;
    use halo2_proofs::circuit::Layouter;
    use halo2_proofs::circuit::SimpleFloorPlanner;
    use halo2_proofs::dev::MockProver;
    use halo2_proofs::pairing::bn256::Fr;
    use halo2_proofs::plonk::Circuit;
    use halo2_proofs::plonk::ConstraintSystem;
    use halo2_proofs::plonk::Error;
    use rand::rngs::OsRng;

    use super::super::primitives::ConstantLength;
    use super::super::primitives::P128Pow5T9 as OrchardNullifier;
    use super::super::primitives::Spec;
    use super::super::primitives::{self as poseidon};
    use super::super::Hash;
    use super::PoseidonInstructions;
    use super::Pow5Chip;
    use super::Pow5Config;
    use super::StateWord;
    use std::convert::TryInto;
    use std::marker::PhantomData;

    struct PermuteCircuit<S: Spec<Fr, WIDTH, RATE>, const WIDTH: usize, const RATE: usize>(
        PhantomData<S>,
    );

    impl<S: Spec<Fr, WIDTH, RATE>, const WIDTH: usize, const RATE: usize> Circuit<Fr>
        for PermuteCircuit<S, WIDTH, RATE>
    {
        type Config = Pow5Config<Fr, WIDTH, RATE>;
        type FloorPlanner = SimpleFloorPlanner;

        fn without_witnesses(&self) -> Self {
            PermuteCircuit::<S, WIDTH, RATE>(PhantomData)
        }

        fn configure(meta: &mut ConstraintSystem<Fr>) -> Pow5Config<Fr, WIDTH, RATE> {
            let state = (0..WIDTH).map(|_| meta.advice_column()).collect::<Vec<_>>();
            let partial_sbox = meta.advice_column();
            let mid_0_helper = meta.advice_column();
            let mid_0_helper_sqr = meta.advice_column();
            let cur_0_rc_a0 = meta.advice_column();
            let cur_0_rc_a0_sqr = meta.advice_column();

            let state_rc_a = (0..WIDTH).map(|_| meta.advice_column()).collect::<Vec<_>>();
            let state_rc_a_sqr = (0..WIDTH).map(|_| meta.advice_column()).collect::<Vec<_>>();

            let rc_a = (0..WIDTH).map(|_| meta.fixed_column()).collect::<Vec<_>>();
            let rc_b = (0..WIDTH).map(|_| meta.fixed_column()).collect::<Vec<_>>();

            Pow5Chip::configure::<S>(
                meta,
                state.try_into().unwrap(),
                state_rc_a.try_into().unwrap(),
                state_rc_a_sqr.try_into().unwrap(),
                partial_sbox,
                mid_0_helper,
                mid_0_helper_sqr,
                cur_0_rc_a0,
                cur_0_rc_a0_sqr,
                rc_a.try_into().unwrap(),
                rc_b.try_into().unwrap(),
            )
        }

        fn synthesize(
            &self,
            config: Pow5Config<Fr, WIDTH, RATE>,
            mut layouter: impl Layouter<Fr>,
        ) -> Result<(), Error> {
            let initial_state = layouter.assign_region(
                || "prepare initial state",
                |mut region| {
                    let state_word = |i: usize| {
                        let var = region.assign_advice(
                            || format!("load state_{}", i),
                            config.state[i],
                            0,
                            || Ok(Fr::from(i as u64)),
                        )?;
                        Ok(StateWord(var))
                    };

                    let state: Result<Vec<_>, Error> = (0..WIDTH).map(state_word).collect();
                    Ok(state?.try_into().unwrap())
                },
            )?;

            let chip = Pow5Chip::construct(config.clone());
            let final_state = <Pow5Chip<_, WIDTH, RATE> as PoseidonInstructions<
                Fr,
                S,
                WIDTH,
                RATE,
            >>::permute(&chip, &mut layouter, &initial_state)?;

            // For the purpose of this test, compute the real final state inline.
            let mut expected_final_state = (0..WIDTH)
                .map(|idx| Fr::from(idx as u64))
                .collect::<Vec<_>>()
                .try_into()
                .unwrap();
            let (round_constants, mds, _) = S::constants();
            poseidon::permute::<_, S, WIDTH, RATE>(
                &mut expected_final_state,
                &mds,
                &round_constants,
            );

            layouter.assign_region(
                || "constrain final state",
                |mut region| {
                    let mut final_state_word = |i: usize| {
                        let var = region.assign_advice(
                            || format!("load final_state_{}", i),
                            config.state[i],
                            0,
                            || Ok(expected_final_state[i]),
                        )?;
                        region.constrain_equal(final_state[i].0.cell(), var.cell())
                    };

                    for i in 0..(WIDTH) {
                        final_state_word(i)?;
                    }

                    Ok(())
                },
            )
        }
    }

    #[test]
    fn poseidon_permute() {
        let k = 6;
        let circuit = PermuteCircuit::<OrchardNullifier<Fr>, 9, 8>(PhantomData);
        let prover = MockProver::run(k, &circuit, vec![]).unwrap();
        assert_eq!(prover.verify(), Ok(()))
    }

    struct HashCircuit<
        S: Spec<Fr, WIDTH, RATE>,
        const WIDTH: usize,
        const RATE: usize,
        const L: usize,
    > {
        message: Option<[Fr; L]>,
        // For the purpose of this test, witness the result.
        // TODO: Move this into an instance column.
        output: Option<Fr>,
        _spec: PhantomData<S>,
    }

    impl<S: Spec<Fr, WIDTH, RATE>, const WIDTH: usize, const RATE: usize, const L: usize>
        Circuit<Fr> for HashCircuit<S, WIDTH, RATE, L>
    {
        type Config = Pow5Config<Fr, WIDTH, RATE>;
        type FloorPlanner = SimpleFloorPlanner;

        fn without_witnesses(&self) -> Self {
            Self {
                message: None,
                output: None,
                _spec: PhantomData,
            }
        }

        fn configure(meta: &mut ConstraintSystem<Fr>) -> Pow5Config<Fr, WIDTH, RATE> {
            let state = (0..WIDTH).map(|_| meta.advice_column()).collect::<Vec<_>>();
            let partial_sbox = meta.advice_column();
            let mid_0_helper = meta.advice_column();
            let mid_0_helper_sqr = meta.advice_column();
            let cur_0_rc_a0 = meta.advice_column();
            let cur_0_rc_a0_sqr = meta.advice_column();

            let state_rc_a = (0..WIDTH).map(|_| meta.advice_column()).collect::<Vec<_>>();
            let state_rc_a_sqr = (0..WIDTH).map(|_| meta.advice_column()).collect::<Vec<_>>();

            let rc_a = (0..WIDTH).map(|_| meta.fixed_column()).collect::<Vec<_>>();
            let rc_b = (0..WIDTH).map(|_| meta.fixed_column()).collect::<Vec<_>>();

            Pow5Chip::configure::<S>(
                meta,
                state.try_into().unwrap(),
                state_rc_a.try_into().unwrap(),
                state_rc_a_sqr.try_into().unwrap(),
                partial_sbox,
                mid_0_helper,
                mid_0_helper_sqr,
                cur_0_rc_a0,
                cur_0_rc_a0_sqr,
                rc_a.try_into().unwrap(),
                rc_b.try_into().unwrap(),
            )
        }

        fn synthesize(
            &self,
            config: Pow5Config<Fr, WIDTH, RATE>,
            mut layouter: impl Layouter<Fr>,
        ) -> Result<(), Error> {
            let chip = Pow5Chip::construct(config.clone());

            let message = layouter.assign_region(
                || "load message",
                |mut region| {
                    let message_word = |i: usize| {
                        let value = self.message.map(|message_vals| message_vals[i]);
                        region.assign_advice(
                            || format!("load message_{}", i),
                            config.state[i % WIDTH],
                            i / WIDTH,
                            || Ok(value.unwrap()),
                        )
                    };

                    let message: Result<Vec<_>, Error> = (0..L).map(message_word).collect();
                    Ok(message?.try_into().unwrap())
                },
            )?;

            let hasher = Hash::<_, _, S, ConstantLength<L>, WIDTH, RATE>::init(
                chip,
                layouter.namespace(|| "init"),
            )?;
            let output = hasher.hash(layouter.namespace(|| "hash"), message)?;

            layouter.assign_region(
                || "constrain output",
                |mut region| {
                    let expected_var = region.assign_advice(
                        || "load output",
                        config.state[0],
                        0,
                        || Ok(self.output.unwrap()),
                    )?;
                    region.constrain_equal(output.cell(), expected_var.cell())
                },
            )
        }
    }

    #[test]
    fn poseidon_hash() {
        let rng = OsRng;

        let message = [0; 8].map(|_| Fr::random(rng));
        let output = poseidon::Hash::<_, OrchardNullifier<Fr>, ConstantLength<8>, 9, 8>::init()
            .hash(message);

        let k = 6;
        let circuit = HashCircuit::<OrchardNullifier<Fr>, 9, 8, 8> {
            message: Some(message),
            output: Some(output),
            _spec: PhantomData,
        };
        let prover = MockProver::run(k, &circuit, vec![]).unwrap();
        assert_eq!(prover.verify(), Ok(()))
    }

    #[test]
    fn poseidon_hash_longer_input() {
        let rng = OsRng;

        let message = [0; 13].map(|_| Fr::random(rng));
        let output = poseidon::Hash::<_, OrchardNullifier<Fr>, ConstantLength<13>, 9, 8>::init()
            .hash(message);

        let k = 7;
        let circuit = HashCircuit::<OrchardNullifier<Fr>, 9, 8, 13> {
            message: Some(message),
            output: Some(output),
            _spec: PhantomData,
        };
        let prover = MockProver::run(k, &circuit, vec![]).unwrap();
        assert_eq!(prover.verify(), Ok(()))
    }
}
