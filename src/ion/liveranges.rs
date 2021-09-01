/*
 * The following license applies to this file, which was initially
 * derived from the files `js/src/jit/BacktrackingAllocator.h` and
 * `js/src/jit/BacktrackingAllocator.cpp` in Mozilla Firefox:
 *
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/.
 *
 * Since the initial port, the design has been substantially evolved
 * and optimized.
 */

//! Live-range computation.

use super::{
    CodeRange, Env, InsertMovePrio, LiveBundle, LiveBundleIndex, LiveRange, LiveRangeFlag,
    LiveRangeIndex, LiveRangeKey, LiveRangeListEntry, LiveRangeSet, PRegData, PRegIndex, RegClass,
    SpillSetIndex, Use, VRegData, VRegIndex, SLOT_NONE,
};
use crate::indexset::IndexSet;
use crate::{
    Allocation, Block, Function, Inst, InstPosition, Operand, OperandConstraint, OperandKind,
    OperandPos, PReg, ProgPoint, RegAllocError, VReg,
};
use fxhash::FxHashSet;
use smallvec::{smallvec, SmallVec};
use std::collections::{HashSet, VecDeque};

/// A spill weight computed for a certain Use.
#[derive(Clone, Copy, Debug)]
pub struct SpillWeight(f32);

#[inline(always)]
pub fn spill_weight_from_constraint(
    constraint: OperandConstraint,
    loop_depth: usize,
    is_def: bool,
) -> SpillWeight {
    // A bonus of 1000 for one loop level, 4000 for two loop levels,
    // 16000 for three loop levels, etc. Avoids exponentiation.
    let loop_depth = std::cmp::min(10, loop_depth);
    let hot_bonus: f32 = (0..loop_depth).fold(1000.0, |a, _| a * 4.0);
    let def_bonus: f32 = if is_def { 2000.0 } else { 0.0 };
    let constraint_bonus: f32 = match constraint {
        OperandConstraint::Any => 1000.0,
        OperandConstraint::Reg | OperandConstraint::FixedReg(_) => 2000.0,
        _ => 0.0,
    };
    SpillWeight(hot_bonus + def_bonus + constraint_bonus)
}

impl SpillWeight {
    /// Convert a floating-point weight to a u16 that can be compactly
    /// stored in a `Use`. We simply take the top 16 bits of the f32; this
    /// is equivalent to the bfloat16 format
    /// (https://en.wikipedia.org/wiki/Bfloat16_floating-point_format).
    pub fn to_bits(self) -> u16 {
        (self.0.to_bits() >> 15) as u16
    }

    /// Convert a value that was returned from
    /// `SpillWeight::to_bits()` back into a `SpillWeight`. Note that
    /// some precision may be lost when round-tripping from a spill
    /// weight to packed bits and back.
    pub fn from_bits(bits: u16) -> SpillWeight {
        let x = f32::from_bits((bits as u32) << 15);
        SpillWeight(x)
    }

    /// Get a zero spill weight.
    pub fn zero() -> SpillWeight {
        SpillWeight(0.0)
    }

    /// Convert to a raw floating-point value.
    pub fn to_f32(self) -> f32 {
        self.0
    }

    /// Create a `SpillWeight` from a raw floating-point value.
    pub fn from_f32(x: f32) -> SpillWeight {
        SpillWeight(x)
    }

    pub fn to_int(self) -> u32 {
        self.0 as u32
    }
}

impl std::ops::Add<SpillWeight> for SpillWeight {
    type Output = SpillWeight;
    fn add(self, other: SpillWeight) -> Self {
        SpillWeight(self.0 + other.0)
    }
}

impl<'a, F: Function> Env<'a, F> {
    pub fn create_pregs_and_vregs(&mut self) {
        // Create PRegs from the env.
        self.pregs.resize(
            PReg::MAX_INDEX,
            PRegData {
                reg: PReg::invalid(),
                allocations: LiveRangeSet::new(),
            },
        );
        for &preg in &self.env.regs {
            self.pregs[preg.index()].reg = preg;
        }
        // Create VRegs from the vreg count.
        for idx in 0..self.func.num_vregs() {
            // We'll fill in the real details when we see the def.
            let reg = VReg::new(idx, RegClass::Int);
            self.add_vreg(
                reg,
                VRegData {
                    ranges: smallvec![],
                    blockparam: Block::invalid(),
                    is_ref: false,
                    is_pinned: false,
                },
            );
        }
        for v in self.func.reftype_vregs() {
            self.vregs[v.vreg()].is_ref = true;
        }
        for v in self.func.pinned_vregs() {
            self.vregs[v.vreg()].is_pinned = true;
        }
        // Create allocations too.
        for inst in 0..self.func.num_insts() {
            let start = self.allocs.len() as u32;
            self.inst_alloc_offsets.push(start);
            for _ in 0..self.func.inst_operands(Inst::new(inst)).len() {
                self.allocs.push(Allocation::none());
            }
        }
    }

    pub fn add_vreg(&mut self, reg: VReg, data: VRegData) -> VRegIndex {
        let idx = self.vregs.len();
        self.vregs.push(data);
        self.vreg_regs.push(reg);
        VRegIndex::new(idx)
    }

    pub fn create_bundle(&mut self) -> LiveBundleIndex {
        let bundle = self.bundles.len();
        self.bundles.push(LiveBundle {
            allocation: Allocation::none(),
            ranges: smallvec![],
            spillset: SpillSetIndex::invalid(),
            prio: 0,
            spill_weight_and_props: 0,
        });
        LiveBundleIndex::new(bundle)
    }

    pub fn create_liverange(&mut self, range: CodeRange) -> LiveRangeIndex {
        let idx = self.ranges.len();

        self.ranges.push(LiveRange {
            range,
            vreg: VRegIndex::invalid(),
            bundle: LiveBundleIndex::invalid(),
            uses_spill_weight_and_flags: 0,

            uses: smallvec![],

            merged_into: LiveRangeIndex::invalid(),
        });

        LiveRangeIndex::new(idx)
    }

    /// Mark `range` as live for the given `vreg`.
    ///
    /// Returns the liverange that contains the given range.
    pub fn add_liverange_to_vreg(&mut self, vreg: VRegIndex, range: CodeRange) -> LiveRangeIndex {
        log::trace!("add_liverange_to_vreg: vreg {:?} range {:?}", vreg, range);

        // Invariant: as we are building liveness information, we
        // *always* process instructions bottom-to-top, and as a
        // consequence, new liveranges are always created before any
        // existing liveranges for a given vreg. We assert this here,
        // then use it to avoid an O(n) merge step (which would lead
        // to O(n^2) liveness construction cost overall).
        //
        // We store liveranges in reverse order in the `.ranges`
        // array, then reverse them at the end of
        // `compute_liveness()`.

        assert!(
            self.vregs[vreg.index()].ranges.is_empty()
                || range.to
                    <= self.ranges[self.vregs[vreg.index()]
                        .ranges
                        .last()
                        .unwrap()
                        .index
                        .index()]
                    .range
                    .from
        );

        if self.vregs[vreg.index()].ranges.is_empty()
            || range.to
                < self.ranges[self.vregs[vreg.index()]
                    .ranges
                    .last()
                    .unwrap()
                    .index
                    .index()]
                .range
                .from
        {
            // Is not contiguous with previously-added (immediately
            // following) range; create a new range.
            let lr = self.create_liverange(range);
            self.ranges[lr.index()].vreg = vreg;
            self.vregs[vreg.index()]
                .ranges
                .push(LiveRangeListEntry { range, index: lr });
            lr
        } else {
            // Is contiguous with previously-added range; just extend
            // its range and return it.
            let lr = self.vregs[vreg.index()].ranges.last().unwrap().index;
            assert!(range.to == self.ranges[lr.index()].range.from);
            self.ranges[lr.index()].range.from = range.from;
            lr
        }
    }

    pub fn insert_use_into_liverange(&mut self, into: LiveRangeIndex, mut u: Use) {
        let operand = u.operand;
        let constraint = operand.constraint();
        let block = self.cfginfo.insn_block[u.pos.inst().index()];
        let loop_depth = self.cfginfo.approx_loop_depth[block.index()] as usize;
        let weight = spill_weight_from_constraint(
            constraint,
            loop_depth,
            operand.kind() != OperandKind::Use,
        );
        u.weight = weight.to_bits();

        log::trace!(
            "insert use {:?} into lr {:?} with weight {:?}",
            u,
            into,
            weight,
        );

        // N.B.: we do *not* update `requirement` on the range,
        // because those will be computed during the multi-fixed-reg
        // fixup pass later (after all uses are inserted).

        self.ranges[into.index()].uses.push(u);

        // Update stats.
        let range_weight = self.ranges[into.index()].uses_spill_weight() + weight;
        self.ranges[into.index()].set_uses_spill_weight(range_weight);
        log::trace!(
            "  -> now range has weight {:?}",
            self.ranges[into.index()].uses_spill_weight(),
        );
    }

    pub fn find_vreg_liverange_for_pos(
        &self,
        vreg: VRegIndex,
        pos: ProgPoint,
    ) -> Option<LiveRangeIndex> {
        for entry in &self.vregs[vreg.index()].ranges {
            if entry.range.contains_point(pos) {
                return Some(entry.index);
            }
        }
        None
    }

    pub fn add_liverange_to_preg(&mut self, range: CodeRange, reg: PReg) {
        log::trace!("adding liverange to preg: {:?} to {}", range, reg);
        let preg_idx = PRegIndex::new(reg.index());
        self.pregs[preg_idx.index()]
            .allocations
            .btree
            .insert(LiveRangeKey::from_range(&range), LiveRangeIndex::invalid());
    }

    pub fn is_live_in(&mut self, block: Block, vreg: VRegIndex) -> bool {
        self.liveins[block.index()].get(vreg.index())
    }

    pub fn compute_liveness(&mut self) -> Result<(), RegAllocError> {
        // Create initial LiveIn and LiveOut bitsets.
        for _ in 0..self.func.num_blocks() {
            self.liveins.push(IndexSet::new());
            self.liveouts.push(IndexSet::new());
        }

        // Run a worklist algorithm to precisely compute liveins and
        // liveouts.
        let mut workqueue = VecDeque::new();
        let mut workqueue_set = FxHashSet::default();
        // Initialize workqueue with postorder traversal.
        for &block in &self.cfginfo.postorder[..] {
            workqueue.push_back(block);
            workqueue_set.insert(block);
        }

        while !workqueue.is_empty() {
            let block = workqueue.pop_front().unwrap();
            workqueue_set.remove(&block);

            log::trace!("computing liveins for block{}", block.index());

            self.stats.livein_iterations += 1;

            let mut live = self.liveouts[block.index()].clone();
            log::trace!(" -> initial liveout set: {:?}", live);

            for inst in self.func.block_insns(block).rev().iter() {
                if let Some((src, dst)) = self.func.is_move(inst) {
                    live.set(dst.vreg().vreg(), false);
                    live.set(src.vreg().vreg(), true);
                }

                for pos in &[OperandPos::Late, OperandPos::Early] {
                    for op in self.func.inst_operands(inst) {
                        if op.pos() == *pos {
                            let was_live = live.get(op.vreg().vreg());
                            log::trace!("op {:?} was_live = {}", op, was_live);
                            match op.kind() {
                                OperandKind::Use | OperandKind::Mod => {
                                    live.set(op.vreg().vreg(), true);
                                }
                                OperandKind::Def => {
                                    live.set(op.vreg().vreg(), false);
                                }
                            }
                        }
                    }
                }
            }
            for &blockparam in self.func.block_params(block) {
                live.set(blockparam.vreg(), false);
            }

            for &pred in self.func.block_preds(block) {
                if self.liveouts[pred.index()].union_with(&live) {
                    if !workqueue_set.contains(&pred) {
                        workqueue_set.insert(pred);
                        workqueue.push_back(pred);
                    }
                }
            }

            log::trace!("computed liveins at block{}: {:?}", block.index(), live);
            self.liveins[block.index()] = live;
        }

        // Check that there are no liveins to the entry block. (The
        // client should create a virtual intsruction that defines any
        // PReg liveins if necessary.)
        if self.liveins[self.func.entry_block().index()]
            .iter()
            .next()
            .is_some()
        {
            log::trace!(
                "non-empty liveins to entry block: {:?}",
                self.liveins[self.func.entry_block().index()]
            );
            return Err(RegAllocError::EntryLivein);
        }

        for &vreg in self.func.reftype_vregs() {
            self.safepoints_per_vreg.insert(vreg.vreg(), HashSet::new());
        }

        // Create Uses and Defs referring to VRegs, and place the Uses
        // in LiveRanges.
        //
        // We already computed precise liveouts and liveins for every
        // block above, so we don't need to run an iterative algorithm
        // here; instead, every block's computation is purely local,
        // from end to start.

        // Track current LiveRange for each vreg.
        //
        // Invariant: a stale range may be present here; ranges are
        // only valid if `live.get(vreg)` is true.
        let mut vreg_ranges: Vec<LiveRangeIndex> =
            vec![LiveRangeIndex::invalid(); self.func.num_vregs()];

        for i in (0..self.func.num_blocks()).rev() {
            let block = Block::new(i);

            self.stats.livein_blocks += 1;

            // Init our local live-in set.
            let mut live = self.liveouts[block.index()].clone();

            // Initially, registers are assumed live for the whole block.
            for vreg in live.iter() {
                let range = CodeRange {
                    from: self.cfginfo.block_entry[block.index()],
                    to: self.cfginfo.block_exit[block.index()].next(),
                };
                log::trace!(
                    "vreg {:?} live at end of block --> create range {:?}",
                    VRegIndex::new(vreg),
                    range
                );
                let lr = self.add_liverange_to_vreg(VRegIndex::new(vreg), range);
                vreg_ranges[vreg] = lr;
            }

            // Create vreg data for blockparams.
            for param in self.func.block_params(block) {
                self.vreg_regs[param.vreg()] = *param;
                self.vregs[param.vreg()].blockparam = block;
            }

            let insns = self.func.block_insns(block);

            // If the last instruction is a branch (rather than
            // return), create blockparam_out entries.
            if self.func.is_branch(insns.last()) {
                let operands = self.func.inst_operands(insns.last());
                let mut i = self.func.branch_blockparam_arg_offset(block, insns.last());
                for &succ in self.func.block_succs(block) {
                    for &blockparam in self.func.block_params(succ) {
                        let from_vreg = VRegIndex::new(operands[i].vreg().vreg());
                        let blockparam_vreg = VRegIndex::new(blockparam.vreg());
                        self.blockparam_outs
                            .push((from_vreg, block, succ, blockparam_vreg));
                        i += 1;
                    }
                }
            }

            // For each instruction, in reverse order, process
            // operands and clobbers.
            for inst in insns.rev().iter() {
                if self.func.inst_clobbers(inst).len() > 0 {
                    self.clobbers.push(inst);
                }

                // Mark clobbers with CodeRanges on PRegs.
                for i in 0..self.func.inst_clobbers(inst).len() {
                    // don't borrow `self`
                    let clobber = self.func.inst_clobbers(inst)[i];
                    // Clobber range is at After point only: an
                    // instruction can still take an input in a reg
                    // that it later clobbers. (In other words, the
                    // clobber is like a normal def that never gets
                    // used.)
                    let range = CodeRange {
                        from: ProgPoint::after(inst),
                        to: ProgPoint::before(inst.next()),
                    };
                    self.add_liverange_to_preg(range, clobber);
                }

                // Does the instruction have any input-reusing
                // outputs? This is important below to establish
                // proper interference wrt other inputs.
                let mut reused_input = None;
                for op in self.func.inst_operands(inst) {
                    if let OperandConstraint::Reuse(i) = op.constraint() {
                        reused_input = Some(i);
                        break;
                    }
                }

                // If this is a move, handle specially.
                if let Some((src, dst)) = self.func.is_move(inst) {
                    // We can completely skip the move if it is
                    // trivial (vreg to same vreg).
                    if src.vreg() != dst.vreg() {
                        log::trace!(" -> move inst{}: src {} -> dst {}", inst.index(), src, dst);

                        assert_eq!(src.class(), dst.class());
                        assert_eq!(src.kind(), OperandKind::Use);
                        assert_eq!(src.pos(), OperandPos::Early);
                        assert_eq!(dst.kind(), OperandKind::Def);
                        assert_eq!(dst.pos(), OperandPos::Late);

                        // If both src and dest are pinned, emit the
                        // move right here, right now.
                        if self.vregs[src.vreg().vreg()].is_pinned
                            && self.vregs[dst.vreg().vreg()].is_pinned
                        {
                            // Update LRs.
                            if !live.get(src.vreg().vreg()) {
                                let lr = self.add_liverange_to_vreg(
                                    VRegIndex::new(src.vreg().vreg()),
                                    CodeRange {
                                        from: self.cfginfo.block_entry[block.index()],
                                        to: ProgPoint::after(inst),
                                    },
                                );
                                live.set(src.vreg().vreg(), true);
                                vreg_ranges[src.vreg().vreg()] = lr;
                            }
                            if live.get(dst.vreg().vreg()) {
                                let lr = vreg_ranges[dst.vreg().vreg()];
                                self.ranges[lr.index()].range.from = ProgPoint::after(inst);
                                live.set(dst.vreg().vreg(), false);
                            } else {
                                self.add_liverange_to_vreg(
                                    VRegIndex::new(dst.vreg().vreg()),
                                    CodeRange {
                                        from: ProgPoint::after(inst),
                                        to: ProgPoint::before(inst.next()),
                                    },
                                );
                            }

                            let src_preg = match src.constraint() {
                                OperandConstraint::FixedReg(r) => r,
                                _ => unreachable!(),
                            };
                            let dst_preg = match dst.constraint() {
                                OperandConstraint::FixedReg(r) => r,
                                _ => unreachable!(),
                            };
                            self.insert_move(
                                ProgPoint::before(inst),
                                InsertMovePrio::MultiFixedReg,
                                Allocation::reg(src_preg),
                                Allocation::reg(dst_preg),
                                Some(dst.vreg()),
                            );
                        }
                        // If exactly one of source and dest (but not
                        // both) is a pinned-vreg, convert this into a
                        // ghost use on the other vreg with a FixedReg
                        // constraint.
                        else if self.vregs[src.vreg().vreg()].is_pinned
                            || self.vregs[dst.vreg().vreg()].is_pinned
                        {
                            log::trace!(
                                " -> exactly one of src/dst is pinned; converting to ghost use"
                            );
                            let (preg, vreg, pinned_vreg, kind, pos, progpoint) =
                                if self.vregs[src.vreg().vreg()].is_pinned {
                                    // Source is pinned: this is a def on the dst with a pinned preg.
                                    (
                                        self.func.is_pinned_vreg(src.vreg()).unwrap(),
                                        dst.vreg(),
                                        src.vreg(),
                                        OperandKind::Def,
                                        OperandPos::Late,
                                        ProgPoint::after(inst),
                                    )
                                } else {
                                    // Dest is pinned: this is a use on the src with a pinned preg.
                                    (
                                        self.func.is_pinned_vreg(dst.vreg()).unwrap(),
                                        src.vreg(),
                                        dst.vreg(),
                                        OperandKind::Use,
                                        OperandPos::Early,
                                        ProgPoint::after(inst),
                                    )
                                };
                            let constraint = OperandConstraint::FixedReg(preg);
                            let operand = Operand::new(vreg, constraint, kind, pos);

                            log::trace!(
                                concat!(
                                    " -> preg {:?} vreg {:?} kind {:?} ",
                                    "pos {:?} progpoint {:?} constraint {:?} operand {:?}"
                                ),
                                preg,
                                vreg,
                                kind,
                                pos,
                                progpoint,
                                constraint,
                                operand
                            );

                            // Get the LR for the vreg; if none, create one.
                            let mut lr = vreg_ranges[vreg.vreg()];
                            if !live.get(vreg.vreg()) {
                                let from = match kind {
                                    OperandKind::Use => self.cfginfo.block_entry[block.index()],
                                    OperandKind::Def => progpoint,
                                    _ => unreachable!(),
                                };
                                let to = progpoint.next();
                                lr = self.add_liverange_to_vreg(
                                    VRegIndex::new(vreg.vreg()),
                                    CodeRange { from, to },
                                );
                                log::trace!("   -> dead; created LR");
                            }
                            log::trace!("  -> LR {:?}", lr);

                            self.insert_use_into_liverange(
                                lr,
                                Use::new(operand, progpoint, SLOT_NONE),
                            );

                            if kind == OperandKind::Def {
                                live.set(vreg.vreg(), false);
                                if self.ranges[lr.index()].range.from
                                    == self.cfginfo.block_entry[block.index()]
                                {
                                    self.ranges[lr.index()].range.from = progpoint;
                                }
                                self.ranges[lr.index()].set_flag(LiveRangeFlag::StartsAtDef);
                            } else {
                                live.set(vreg.vreg(), true);
                                vreg_ranges[vreg.vreg()] = lr;
                            }

                            // Handle liveness of the other vreg. Note
                            // that this is somewhat special. For the
                            // destination case, we want the pinned
                            // vreg's LR to start just *after* the
                            // operand we inserted above, because
                            // otherwise it would overlap, and
                            // interfere, and prevent allocation. For
                            // the source case, we want to "poke a
                            // hole" in the LR: if it's live going
                            // downward, end it just after the operand
                            // and restart it before; if it isn't
                            // (this is the last use), start it
                            // before.
                            if kind == OperandKind::Def {
                                log::trace!(" -> src on pinned vreg {:?}", pinned_vreg);
                                // The *other* vreg is a def, so the pinned-vreg
                                // mention is a use. If already live,
                                // end the existing LR just *after*
                                // the `progpoint` defined above and
                                // start a new one just *before* the
                                // `progpoint` defined above,
                                // preserving the start. If not, start
                                // a new one live back to the top of
                                // the block, starting just before
                                // `progpoint`.
                                if live.get(pinned_vreg.vreg()) {
                                    let pinned_lr = vreg_ranges[pinned_vreg.vreg()];
                                    let orig_start = self.ranges[pinned_lr.index()].range.from;
                                    log::trace!(
                                        " -> live with LR {:?}; truncating to start at {:?}",
                                        pinned_lr,
                                        progpoint.next()
                                    );
                                    self.ranges[pinned_lr.index()].range.from = progpoint.next();
                                    let new_lr = self.add_liverange_to_vreg(
                                        VRegIndex::new(pinned_vreg.vreg()),
                                        CodeRange {
                                            from: orig_start,
                                            to: progpoint.prev(),
                                        },
                                    );
                                    vreg_ranges[pinned_vreg.vreg()] = new_lr;
                                    log::trace!(" -> created LR {:?} with remaining range from {:?} to {:?}", new_lr, orig_start, progpoint);

                                    // Add an edit right now to indicate that at
                                    // this program point, the given
                                    // preg is now known as that vreg,
                                    // not the preg, but immediately
                                    // after, it is known as the preg
                                    // again. This is used by the
                                    // checker.
                                    self.insert_move(
                                        ProgPoint::after(inst),
                                        InsertMovePrio::Regular,
                                        Allocation::reg(preg),
                                        Allocation::reg(preg),
                                        Some(dst.vreg()),
                                    );
                                    self.insert_move(
                                        ProgPoint::before(inst.next()),
                                        InsertMovePrio::MultiFixedReg,
                                        Allocation::reg(preg),
                                        Allocation::reg(preg),
                                        Some(src.vreg()),
                                    );
                                } else {
                                    if inst > self.cfginfo.block_entry[block.index()].inst() {
                                        let new_lr = self.add_liverange_to_vreg(
                                            VRegIndex::new(pinned_vreg.vreg()),
                                            CodeRange {
                                                from: self.cfginfo.block_entry[block.index()],
                                                to: ProgPoint::before(inst),
                                            },
                                        );
                                        vreg_ranges[pinned_vreg.vreg()] = new_lr;
                                        live.set(pinned_vreg.vreg(), true);
                                        log::trace!(
                                            " -> was not live; created new LR {:?}",
                                            new_lr
                                        );
                                    }

                                    // Add an edit right now to indicate that at
                                    // this program point, the given
                                    // preg is now known as that vreg,
                                    // not the preg. This is used by
                                    // the checker.
                                    self.insert_move(
                                        ProgPoint::after(inst),
                                        InsertMovePrio::BlockParam,
                                        Allocation::reg(preg),
                                        Allocation::reg(preg),
                                        Some(dst.vreg()),
                                    );
                                }
                            } else {
                                log::trace!(" -> dst on pinned vreg {:?}", pinned_vreg);
                                // The *other* vreg is a use, so the pinned-vreg
                                // mention is a def. Truncate its LR
                                // just *after* the `progpoint`
                                // defined above.
                                if live.get(pinned_vreg.vreg()) {
                                    let pinned_lr = vreg_ranges[pinned_vreg.vreg()];
                                    self.ranges[pinned_lr.index()].range.from = progpoint.next();
                                    log::trace!(
                                        " -> was live with LR {:?}; truncated start to {:?}",
                                        pinned_lr,
                                        progpoint.next()
                                    );
                                    live.set(pinned_vreg.vreg(), false);

                                    // Add a no-op edit right now to indicate that
                                    // at this program point, the
                                    // given preg is now known as that
                                    // preg, not the vreg. This is
                                    // used by the checker.
                                    self.insert_move(
                                        ProgPoint::before(inst.next()),
                                        InsertMovePrio::PostRegular,
                                        Allocation::reg(preg),
                                        Allocation::reg(preg),
                                        Some(dst.vreg()),
                                    );
                                }
                                // Otherwise, if dead, no need to create
                                // a dummy LR -- there is no
                                // reservation to make (the other vreg
                                // will land in the reg with the
                                // fixed-reg operand constraint, but
                                // it's a dead move anyway).
                            }
                        } else {
                            // Redefine src and dst operands to have
                            // positions of After and Before respectively
                            // (see note below), and to have Any
                            // constraints if they were originally Reg.
                            let src_constraint = match src.constraint() {
                                OperandConstraint::Reg => OperandConstraint::Any,
                                x => x,
                            };
                            let dst_constraint = match dst.constraint() {
                                OperandConstraint::Reg => OperandConstraint::Any,
                                x => x,
                            };
                            let src = Operand::new(
                                src.vreg(),
                                src_constraint,
                                OperandKind::Use,
                                OperandPos::Late,
                            );
                            let dst = Operand::new(
                                dst.vreg(),
                                dst_constraint,
                                OperandKind::Def,
                                OperandPos::Early,
                            );

                            if self.annotations_enabled {
                                self.annotate(
                                    ProgPoint::after(inst),
                                    format!(
                                        " prog-move v{} ({:?}) -> v{} ({:?})",
                                        src.vreg().vreg(),
                                        src_constraint,
                                        dst.vreg().vreg(),
                                        dst_constraint,
                                    ),
                                );
                            }

                            // N.B.: in order to integrate with the move
                            // resolution that joins LRs in general, we
                            // conceptually treat the move as happening
                            // between the move inst's After and the next
                            // inst's Before. Thus the src LR goes up to
                            // (exclusive) next-inst-pre, and the dst LR
                            // starts at next-inst-pre. We have to take
                            // care in our move insertion to handle this
                            // like other inter-inst moves, i.e., at
                            // `Regular` priority, so it properly happens
                            // in parallel with other inter-LR moves.
                            //
                            // Why the progpoint between move and next
                            // inst, and not the progpoint between prev
                            // inst and move? Because a move can be the
                            // first inst in a block, but cannot be the
                            // last; so the following progpoint is always
                            // within the same block, while the previous
                            // one may be an inter-block point (and the
                            // After of the prev inst in a different
                            // block).

                            // Handle the def w.r.t. liveranges: trim the
                            // start of the range and mark it dead at this
                            // point in our backward scan.
                            let pos = ProgPoint::before(inst.next());
                            let mut dst_lr = vreg_ranges[dst.vreg().vreg()];
                            if !live.get(dst.vreg().vreg()) {
                                let from = pos;
                                let to = pos.next();
                                dst_lr = self.add_liverange_to_vreg(
                                    VRegIndex::new(dst.vreg().vreg()),
                                    CodeRange { from, to },
                                );
                                log::trace!(" -> invalid LR for def; created {:?}", dst_lr);
                            }
                            log::trace!(" -> has existing LR {:?}", dst_lr);
                            // Trim the LR to start here.
                            if self.ranges[dst_lr.index()].range.from
                                == self.cfginfo.block_entry[block.index()]
                            {
                                log::trace!(" -> started at block start; trimming to {:?}", pos);
                                self.ranges[dst_lr.index()].range.from = pos;
                            }
                            self.ranges[dst_lr.index()].set_flag(LiveRangeFlag::StartsAtDef);
                            live.set(dst.vreg().vreg(), false);
                            vreg_ranges[dst.vreg().vreg()] = LiveRangeIndex::invalid();
                            self.vreg_regs[dst.vreg().vreg()] = dst.vreg();

                            // Handle the use w.r.t. liveranges: make it live
                            // and create an initial LR back to the start of
                            // the block.
                            let pos = ProgPoint::after(inst);
                            let src_lr = if !live.get(src.vreg().vreg()) {
                                let range = CodeRange {
                                    from: self.cfginfo.block_entry[block.index()],
                                    to: pos.next(),
                                };
                                let src_lr = self.add_liverange_to_vreg(
                                    VRegIndex::new(src.vreg().vreg()),
                                    range,
                                );
                                vreg_ranges[src.vreg().vreg()] = src_lr;
                                src_lr
                            } else {
                                vreg_ranges[src.vreg().vreg()]
                            };

                            log::trace!(" -> src LR {:?}", src_lr);

                            // Add to live-set.
                            let src_is_dead_after_move = !live.get(src.vreg().vreg());
                            live.set(src.vreg().vreg(), true);

                            // Add to program-moves lists.
                            self.prog_move_srcs.push((
                                (VRegIndex::new(src.vreg().vreg()), inst),
                                Allocation::none(),
                            ));
                            self.prog_move_dsts.push((
                                (VRegIndex::new(dst.vreg().vreg()), inst.next()),
                                Allocation::none(),
                            ));
                            self.stats.prog_moves += 1;
                            if src_is_dead_after_move {
                                self.stats.prog_moves_dead_src += 1;
                                self.prog_move_merges.push((src_lr, dst_lr));
                            }
                        }
                    }

                    continue;
                }

                // Process defs and uses.
                for &cur_pos in &[InstPosition::After, InstPosition::Before] {
                    for i in 0..self.func.inst_operands(inst).len() {
                        // don't borrow `self`
                        let operand = self.func.inst_operands(inst)[i];
                        let pos = match (operand.kind(), operand.pos()) {
                            (OperandKind::Mod, _) => ProgPoint::before(inst),
                            (OperandKind::Def, OperandPos::Early) => ProgPoint::before(inst),
                            (OperandKind::Def, OperandPos::Late) => ProgPoint::after(inst),
                            (OperandKind::Use, OperandPos::Late) => ProgPoint::after(inst),
                            // If this is a branch, extend `pos` to
                            // the end of the block. (Branch uses are
                            // blockparams and need to be live at the
                            // end of the block.)
                            (OperandKind::Use, _) if self.func.is_branch(inst) => {
                                self.cfginfo.block_exit[block.index()]
                            }
                            // If there are any reused inputs in this
                            // instruction, and this is *not* the
                            // reused input, force `pos` to
                            // `After`. (See note below for why; it's
                            // very subtle!)
                            (OperandKind::Use, OperandPos::Early)
                                if reused_input.is_some() && reused_input.unwrap() != i =>
                            {
                                ProgPoint::after(inst)
                            }
                            (OperandKind::Use, OperandPos::Early) => ProgPoint::before(inst),
                        };

                        if pos.pos() != cur_pos {
                            continue;
                        }

                        log::trace!(
                            "processing inst{} operand at {:?}: {:?}",
                            inst.index(),
                            pos,
                            operand
                        );

                        match operand.kind() {
                            OperandKind::Def | OperandKind::Mod => {
                                log::trace!("Def of {} at {:?}", operand.vreg(), pos);

                                // Fill in vreg's actual data.
                                self.vreg_regs[operand.vreg().vreg()] = operand.vreg();

                                // Get or create the LiveRange.
                                let mut lr = vreg_ranges[operand.vreg().vreg()];
                                log::trace!(" -> has existing LR {:?}", lr);
                                // If there was no liverange (dead def), create a trivial one.
                                if !live.get(operand.vreg().vreg()) {
                                    let from = match operand.kind() {
                                        OperandKind::Def => pos,
                                        OperandKind::Mod => self.cfginfo.block_entry[block.index()],
                                        _ => unreachable!(),
                                    };
                                    let to = match operand.kind() {
                                        OperandKind::Def => pos.next(),
                                        OperandKind::Mod => pos.next().next(), // both Before and After positions
                                        _ => unreachable!(),
                                    };
                                    lr = self.add_liverange_to_vreg(
                                        VRegIndex::new(operand.vreg().vreg()),
                                        CodeRange { from, to },
                                    );
                                    log::trace!(" -> invalid; created {:?}", lr);
                                    vreg_ranges[operand.vreg().vreg()] = lr;
                                    live.set(operand.vreg().vreg(), true);
                                }
                                // Create the use in the LiveRange.
                                self.insert_use_into_liverange(lr, Use::new(operand, pos, i as u8));
                                // If def (not mod), this reg is now dead,
                                // scanning backward; make it so.
                                if operand.kind() == OperandKind::Def {
                                    // Trim the range for this vreg to start
                                    // at `pos` if it previously ended at the
                                    // start of this block (i.e. was not
                                    // merged into some larger LiveRange due
                                    // to out-of-order blocks).
                                    if self.ranges[lr.index()].range.from
                                        == self.cfginfo.block_entry[block.index()]
                                    {
                                        log::trace!(
                                            " -> started at block start; trimming to {:?}",
                                            pos
                                        );
                                        self.ranges[lr.index()].range.from = pos;
                                    }

                                    self.ranges[lr.index()].set_flag(LiveRangeFlag::StartsAtDef);

                                    // Remove from live-set.
                                    live.set(operand.vreg().vreg(), false);
                                    vreg_ranges[operand.vreg().vreg()] = LiveRangeIndex::invalid();
                                }
                            }
                            OperandKind::Use => {
                                // Create/extend the LiveRange if it
                                // doesn't already exist, and add the use
                                // to the range.
                                let mut lr = vreg_ranges[operand.vreg().vreg()];
                                if !live.get(operand.vreg().vreg()) {
                                    let range = CodeRange {
                                        from: self.cfginfo.block_entry[block.index()],
                                        to: pos.next(),
                                    };
                                    lr = self.add_liverange_to_vreg(
                                        VRegIndex::new(operand.vreg().vreg()),
                                        range,
                                    );
                                    vreg_ranges[operand.vreg().vreg()] = lr;
                                }
                                assert!(lr.is_valid());

                                log::trace!("Use of {:?} at {:?} -> {:?}", operand, pos, lr,);

                                self.insert_use_into_liverange(lr, Use::new(operand, pos, i as u8));

                                // Add to live-set.
                                live.set(operand.vreg().vreg(), true);
                            }
                        }
                    }
                }

                if self.func.requires_refs_on_stack(inst) {
                    log::trace!("inst{} is safepoint", inst.index());
                    self.safepoints.push(inst);
                    for vreg in live.iter() {
                        if let Some(safepoints) = self.safepoints_per_vreg.get_mut(&vreg) {
                            log::trace!("vreg v{} live at safepoint inst{}", vreg, inst.index());
                            safepoints.insert(inst);
                        }
                    }
                }
            }

            // Block parameters define vregs at the very beginning of
            // the block. Remove their live vregs from the live set
            // here.
            for vreg in self.func.block_params(block) {
                if live.get(vreg.vreg()) {
                    live.set(vreg.vreg(), false);
                } else {
                    // Create trivial liverange if blockparam is dead.
                    let start = self.cfginfo.block_entry[block.index()];
                    self.add_liverange_to_vreg(
                        VRegIndex::new(vreg.vreg()),
                        CodeRange {
                            from: start,
                            to: start.next(),
                        },
                    );
                }
                // add `blockparam_ins` entries.
                let vreg_idx = VRegIndex::new(vreg.vreg());
                for &pred in self.func.block_preds(block) {
                    self.blockparam_ins.push((vreg_idx, block, pred));
                }
            }
        }

        self.safepoints.sort_unstable();

        // Make ranges in each vreg and uses in each range appear in
        // sorted order. We built them in reverse order above, so this
        // is a simple reversal, *not* a full sort.
        //
        // The ordering invariant is always maintained for uses and
        // always for ranges in bundles (which are initialized later),
        // but not always for ranges in vregs; those are sorted only
        // when needed, here and then again at the end of allocation
        // when resolving moves.

        for vreg in &mut self.vregs {
            vreg.ranges.reverse();
            let mut last = None;
            for entry in &mut vreg.ranges {
                // Ranges may have been truncated above at defs. We
                // need to update with the final range here.
                entry.range = self.ranges[entry.index.index()].range;
                // Assert in-order and non-overlapping.
                assert!(last.is_none() || last.unwrap() <= entry.range.from);
                last = Some(entry.range.to);
            }
        }

        for range in 0..self.ranges.len() {
            self.ranges[range].uses.reverse();
            debug_assert!(self.ranges[range]
                .uses
                .windows(2)
                .all(|win| win[0].pos <= win[1].pos));
        }

        // Insert safepoint virtual stack uses, if needed.
        for vreg in self.func.reftype_vregs() {
            if self.vregs[vreg.vreg()].is_pinned {
                continue;
            }
            let vreg = VRegIndex::new(vreg.vreg());
            let mut inserted = false;
            let mut safepoint_idx = 0;
            for range_idx in 0..self.vregs[vreg.index()].ranges.len() {
                let LiveRangeListEntry { range, index } =
                    self.vregs[vreg.index()].ranges[range_idx];
                while safepoint_idx < self.safepoints.len()
                    && ProgPoint::before(self.safepoints[safepoint_idx]) < range.from
                {
                    safepoint_idx += 1;
                }
                while safepoint_idx < self.safepoints.len()
                    && range.contains_point(ProgPoint::before(self.safepoints[safepoint_idx]))
                {
                    // Create a virtual use.
                    let pos = ProgPoint::before(self.safepoints[safepoint_idx]);
                    let operand = Operand::new(
                        self.vreg_regs[vreg.index()],
                        OperandConstraint::Stack,
                        OperandKind::Use,
                        OperandPos::Early,
                    );

                    log::trace!(
                        "Safepoint-induced stack use of {:?} at {:?} -> {:?}",
                        operand,
                        pos,
                        index,
                    );

                    self.insert_use_into_liverange(index, Use::new(operand, pos, SLOT_NONE));
                    safepoint_idx += 1;

                    inserted = true;
                }

                if inserted {
                    self.ranges[index.index()]
                        .uses
                        .sort_unstable_by_key(|u| u.pos);
                }

                if safepoint_idx >= self.safepoints.len() {
                    break;
                }
            }
        }

        // Do a fixed-reg cleanup pass: if there are any LiveRanges with
        // multiple uses (or defs) at the same ProgPoint and there is
        // more than one FixedReg constraint at that ProgPoint, we
        // need to record all but one of them in a special fixup list
        // and handle them later; otherwise, bundle-splitting to
        // create minimal bundles becomes much more complex (we would
        // have to split the multiple uses at the same progpoint into
        // different bundles, which breaks invariants related to
        // disjoint ranges and bundles).
        let mut seen_fixed_for_vreg: SmallVec<[VReg; 16]> = smallvec![];
        let mut first_preg: SmallVec<[PRegIndex; 16]> = smallvec![];
        let mut extra_clobbers: SmallVec<[(PReg, Inst); 8]> = smallvec![];
        for vreg in 0..self.vregs.len() {
            for range_idx in 0..self.vregs[vreg].ranges.len() {
                let entry = self.vregs[vreg].ranges[range_idx];
                let range = entry.index;
                log::trace!(
                    "multi-fixed-reg cleanup: vreg {:?} range {:?}",
                    VRegIndex::new(vreg),
                    range,
                );
                let mut last_point = None;
                let mut fixup_multi_fixed_vregs = |pos: ProgPoint,
                                                   slot: usize,
                                                   op: &mut Operand,
                                                   fixups: &mut Vec<(
                    ProgPoint,
                    PRegIndex,
                    PRegIndex,
                    usize,
                )>| {
                    if last_point.is_some() && Some(pos) != last_point {
                        seen_fixed_for_vreg.clear();
                        first_preg.clear();
                    }
                    last_point = Some(pos);

                    if let OperandConstraint::FixedReg(preg) = op.constraint() {
                        let vreg_idx = VRegIndex::new(op.vreg().vreg());
                        let preg_idx = PRegIndex::new(preg.index());
                        log::trace!(
                            "at pos {:?}, vreg {:?} has fixed constraint to preg {:?}",
                            pos,
                            vreg_idx,
                            preg_idx
                        );
                        if let Some(idx) = seen_fixed_for_vreg.iter().position(|r| *r == op.vreg())
                        {
                            let orig_preg = first_preg[idx];
                            if orig_preg != preg_idx {
                                log::trace!(" -> duplicate; switching to constraint Reg");
                                fixups.push((pos, orig_preg, preg_idx, slot));
                                *op = Operand::new(
                                    op.vreg(),
                                    OperandConstraint::Reg,
                                    op.kind(),
                                    op.pos(),
                                );
                                log::trace!(
                                    " -> extra clobber {} at inst{}",
                                    preg,
                                    pos.inst().index()
                                );
                                extra_clobbers.push((preg, pos.inst()));
                            }
                        } else {
                            seen_fixed_for_vreg.push(op.vreg());
                            first_preg.push(preg_idx);
                        }
                    }
                };

                for u in &mut self.ranges[range.index()].uses {
                    let pos = u.pos;
                    let slot = u.slot as usize;
                    fixup_multi_fixed_vregs(
                        pos,
                        slot,
                        &mut u.operand,
                        &mut self.multi_fixed_reg_fixups,
                    );
                }

                for &(clobber, inst) in &extra_clobbers {
                    let range = CodeRange {
                        from: ProgPoint::before(inst),
                        to: ProgPoint::before(inst.next()),
                    };
                    self.add_liverange_to_preg(range, clobber);
                }

                extra_clobbers.clear();
                first_preg.clear();
                seen_fixed_for_vreg.clear();
            }
        }

        self.clobbers.sort_unstable();
        self.blockparam_ins.sort_unstable();
        self.blockparam_outs.sort_unstable();
        self.prog_move_srcs.sort_unstable_by_key(|(pos, _)| *pos);
        self.prog_move_dsts.sort_unstable_by_key(|(pos, _)| *pos);

        log::trace!("prog_move_srcs = {:?}", self.prog_move_srcs);
        log::trace!("prog_move_dsts = {:?}", self.prog_move_dsts);

        self.stats.initial_liverange_count = self.ranges.len();
        self.stats.blockparam_ins_count = self.blockparam_ins.len();
        self.stats.blockparam_outs_count = self.blockparam_outs.len();

        Ok(())
    }
}