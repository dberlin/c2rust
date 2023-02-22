use super::DataflowConstraints;
use crate::context::{AnalysisCtxt, LTy, PermissionSet, PointerId};
use crate::util::{describe_rvalue, ty_callee, Callee, RvalueDesc};
use rustc_hir::def_id::DefId;
use rustc_middle::mir::{
    AggregateKind, BinOp, Body, Location, Mutability, Operand, Place, PlaceRef, ProjectionElem,
    Rvalue, Statement, StatementKind, Terminator, TerminatorKind,
};
use rustc_middle::ty::{SubstsRef, TyKind};

/// Visitor that walks over the MIR, computing types of rvalues/operands/places and generating
/// constraints as a side effect.
///
/// In general, the constraints we generate for an assignment are as follows:
///
/// * The outermost pointer type of the destination must have a subset of the permissions of the
///   outermost pointer type of the source.  That is, the assignment may drop permissions as it
///   copies the pointer from source to destination, but it cannot add any permissions.  Dropping
///   permissions during the assignment corresponds to inserting a cast between pointer types.
/// * All pointer types except the outermost must have equal permissions and flags in the source
///   and destination.  This is necessary because we generally can't change the inner pointer type
///   when performing a cast (for example, it's possible to convert `&[&[T]]` to `&&[T]` - take the
///   address of the first element - but not to `&[&T]]`).
struct TypeChecker<'tcx, 'a> {
    acx: &'a AnalysisCtxt<'a, 'tcx>,
    mir: &'a Body<'tcx>,
    /// Subset constraints on pointer permissions.  For example, this contains constraints like
    /// "the `PermissionSet` assigned to `PointerId` `l1` must be a subset of the `PermissionSet`
    /// assigned to `l2`".  See `dataflow::Constraint` for a full description of supported
    /// constraints.
    constraints: DataflowConstraints,
    /// Equivalence constraints on pointer permissions and flags.  An entry `(l1, l2)` in this list
    /// means that `PointerId`s `l1` and `l2` should be assigned exactly the same permissions and
    /// flags.  This ensures that the two pointers will be rewritten to the same safe type.
    ///
    /// Higher-level code eventually feeds the constraints recorded here into the union-find data
    /// structure defined in `crate::equiv`, so adding a constraint here has the effect of unifying
    /// the equivalence classes of the two `PointerId`s.
    equiv_constraints: Vec<(PointerId, PointerId)>,
}

impl<'tcx> TypeChecker<'tcx, '_> {
    fn add_edge(&mut self, src: PointerId, dest: PointerId) {
        // Copying `src` to `dest` can discard permissions, but can't add new ones.
        self.constraints.add_subset(dest, src);
    }

    fn add_equiv(&mut self, a: PointerId, b: PointerId) {
        self.equiv_constraints.push((a, b));
    }

    fn record_access(&mut self, ptr: PointerId, mutbl: Mutability) {
        eprintln!("record_access({:?}, {:?})", ptr, mutbl);
        if ptr == PointerId::NONE {
            return;
        }
        match mutbl {
            Mutability::Mut => {
                self.constraints
                    .add_all_perms(ptr, PermissionSet::READ | PermissionSet::WRITE);
            }
            Mutability::Not => {
                self.constraints.add_all_perms(ptr, PermissionSet::READ);
            }
        }
    }

    pub fn visit_place(&mut self, pl: Place<'tcx>, mutbl: Mutability) {
        self.visit_place_ref(pl.as_ref(), mutbl)
    }

    pub fn visit_place_ref(&mut self, pl: PlaceRef<'tcx>, mutbl: Mutability) {
        let mut lty = self.acx.type_of(pl.local);
        let mut prev_deref_ptr = None;

        for proj in pl.projection {
            if let ProjectionElem::Deref = proj {
                // All derefs except the last are loads, to retrieve the pointer for the next
                // deref.  However, if the overall `Place` is used mutably (as indicated by
                // `mutbl`), then the previous derefs must be `&mut` as well.  The last deref
                // may not be a memory access at all; for example, `&(*p).x` does not actually
                // access the memory at `*p`.
                if let Some(ptr) = prev_deref_ptr.take() {
                    self.record_access(ptr, mutbl);
                }
                prev_deref_ptr = Some(lty.label);
            }
            lty = self.acx.project(lty, proj);
        }

        if let Some(ptr) = prev_deref_ptr.take() {
            self.record_access(ptr, mutbl);
        }
    }

    pub fn visit_rvalue(&mut self, rv: &Rvalue<'tcx>, lty: LTy<'tcx>) {
        let rv_desc = describe_rvalue(rv);
        eprintln!("visit_rvalue({rv:?}), desc = {rv_desc:?}");

        if let Some(desc) = rv_desc {
            match desc {
                RvalueDesc::Project { base, proj: _ } => {
                    // TODO: mutability should probably depend on mutability of the output ref/ptr
                    self.visit_place_ref(base, Mutability::Not);
                }
                RvalueDesc::AddrOfLocal { .. } => {}
            }
            return;
        }

        match *rv {
            Rvalue::Use(ref op) => self.visit_operand(op),
            Rvalue::Repeat(..) => todo!("visit_rvalue Repeat"),
            Rvalue::Ref(..) => {
                unreachable!("Rvalue::Ref should be handled by describe_rvalue instead")
            }
            Rvalue::ThreadLocalRef(..) => todo!("visit_rvalue ThreadLocalRef"),
            Rvalue::AddressOf(..) => {
                unreachable!("Rvalue::AddressOf should be handled by describe_rvalue instead")
            }
            Rvalue::Len(pl) => {
                self.visit_place(pl, Mutability::Not);
            }
            Rvalue::Cast(_, ref op, _) => self.visit_operand(op),
            Rvalue::BinaryOp(BinOp::Offset, _) => todo!("visit_rvalue BinOp::Offset"),
            Rvalue::BinaryOp(_, ref ops) => {
                self.visit_operand(&ops.0);
                self.visit_operand(&ops.1);
            }
            Rvalue::CheckedBinaryOp(BinOp::Offset, _) => todo!("visit_rvalue BinOp::Offset"),
            Rvalue::CheckedBinaryOp(_, ref ops) => {
                self.visit_operand(&ops.0);
                self.visit_operand(&ops.1);
            }
            Rvalue::NullaryOp(..) => {}
            Rvalue::UnaryOp(_, ref op) => {
                self.visit_operand(op);
            }

            Rvalue::Aggregate(ref kind, ref ops) => {
                for op in ops {
                    self.visit_operand(op);
                }
                match **kind {
                    AggregateKind::Array(..) => {
                        assert!(matches!(lty.kind(), TyKind::Array(..)));
                        assert_eq!(lty.args.len(), 1);
                        let elem_lty = lty.args[0];
                        // Pseudo-assign from each operand to the element type of the array.
                        for op in ops {
                            let op_lty = self.acx.type_of(op);
                            self.do_assign(elem_lty, op_lty);
                        }
                    }
                    ref kind => todo!("Rvalue::Aggregate({:?})", kind),
                }
            }

            _ => panic!("TODO: handle assignment of {:?}", rv),
        }
    }

    pub fn visit_operand(&mut self, op: &Operand<'tcx>) {
        match *op {
            Operand::Copy(pl) | Operand::Move(pl) => {
                self.visit_place(pl, Mutability::Not);
            }
            Operand::Constant(ref _c) => {
                // TODO: addr of static may show up as `Operand::Constant`
            }
        }
    }

    fn do_equivalence_nested(&mut self, pl_lty: LTy<'tcx>, rv_lty: LTy<'tcx>) {
        // Add equivalence constraints for all nested pointers beyond the top level.
        assert_eq!(
            self.acx.tcx().erase_regions(pl_lty.ty),
            self.acx.tcx().erase_regions(rv_lty.ty)
        );
        for (&pl_sub_lty, &rv_sub_lty) in pl_lty.args.iter().zip(rv_lty.args.iter()) {
            self.do_unify(pl_sub_lty, rv_sub_lty);
        }
    }

    fn do_assign(&mut self, pl_lty: LTy<'tcx>, rv_lty: LTy<'tcx>) {
        // If the top-level types are pointers, add a dataflow edge indicating that `rv` flows into
        // `pl`.
        self.do_assign_pointer_ids(pl_lty.label, rv_lty.label);

        self.do_equivalence_nested(pl_lty, rv_lty);
    }

    /// Add a dataflow edge indicating that `rv_ptr` flows into `pl_ptr`.  If both `PointerId`s are
    /// `NONE`, this has no effect.
    fn do_assign_pointer_ids(&mut self, pl_ptr: PointerId, rv_ptr: PointerId) {
        if pl_ptr != PointerId::NONE || rv_ptr != PointerId::NONE {
            assert!(pl_ptr != PointerId::NONE);
            assert!(rv_ptr != PointerId::NONE);
            self.add_edge(rv_ptr, pl_ptr);
        }
    }

    /// Unify corresponding `PointerId`s in `lty1` and `lty2`.
    ///
    /// The two inputs must have identical underlying types.  For any position where the underlying
    /// type has a pointer, this function unifies the `PointerId`s that `lty1` and `lty2` have at
    /// that position.  For example, given `lty1 = *mut /*l1*/ *const /*l2*/ u8` and `lty2 = *mut
    /// /*l3*/ *const /*l4*/ u8`, this function will unify `l1` with `l3` and `l2` with `l4`.
    fn do_unify(&mut self, lty1: LTy<'tcx>, lty2: LTy<'tcx>) {
        assert_eq!(
            self.acx.tcx().erase_regions(lty1.ty),
            self.acx.tcx().erase_regions(lty2.ty)
        );
        for (sub_lty1, sub_lty2) in lty1.iter().zip(lty2.iter()) {
            eprintln!("equate {:?} = {:?}", sub_lty1, sub_lty2);
            if sub_lty1.label != PointerId::NONE || sub_lty2.label != PointerId::NONE {
                assert!(sub_lty1.label != PointerId::NONE);
                assert!(sub_lty2.label != PointerId::NONE);
                self.add_equiv(sub_lty1.label, sub_lty2.label);
            }
        }
    }

    pub fn visit_statement(&mut self, stmt: &Statement<'tcx>, loc: Location) {
        eprintln!("visit_statement({:?})", stmt);
        // TODO(spernsteiner): other `StatementKind`s will be handled in the future
        #[allow(clippy::single_match)]
        match stmt.kind {
            StatementKind::Assign(ref x) => {
                let (pl, ref rv) = **x;
                self.visit_place(pl, Mutability::Mut);
                let pl_lty = self.acx.type_of(pl);

                let rv_lty = self.acx.type_of_rvalue(rv, loc);
                self.visit_rvalue(rv, rv_lty);

                self.do_assign(pl_lty, rv_lty);
            }
            // TODO(spernsteiner): handle other `StatementKind`s
            _ => (),
        }
    }

    pub fn visit_terminator(&mut self, term: &Terminator<'tcx>) {
        eprintln!("visit_terminator({:?})", term.kind);
        let tcx = self.acx.tcx();
        // TODO(spernsteiner): other `TerminatorKind`s will be handled in the future
        #[allow(clippy::single_match)]
        match term.kind {
            TerminatorKind::Call {
                ref func,
                ref args,
                destination,
                target: _,
                ..
            } => {
                let func_ty = func.ty(self.mir, tcx);
                let callee = ty_callee(tcx, func_ty);
                eprintln!("callee = {callee:?}");
                match callee {
                    Callee::Trivial => {}

                    Callee::PtrOffset { .. } => {
                        // We handle this like a pointer assignment.
                        self.visit_place(destination, Mutability::Mut);
                        let pl_lty = self.acx.type_of(destination);
                        assert!(args.len() == 2);
                        self.visit_operand(&args[0]);
                        let rv_lty = self.acx.type_of(&args[0]);
                        self.do_assign(pl_lty, rv_lty);
                        let perms = PermissionSet::OFFSET_ADD | PermissionSet::OFFSET_SUB;
                        self.constraints.add_all_perms(rv_lty.label, perms);
                    }

                    Callee::SliceAsPtr { elem_ty, .. } => {
                        // We handle this like an assignment, but with some adjustments due to the
                        // difference in input and output types.
                        self.visit_place(destination, Mutability::Mut);
                        let pl_lty = self.acx.type_of(destination);
                        assert!(args.len() == 1);
                        self.visit_operand(&args[0]);
                        let rv_lty = self.acx.type_of(&args[0]);

                        // Map `rv_lty = &[i32]` to `rv_elem_lty = i32`
                        let rv_pointee_lty = rv_lty.args[0];
                        let rv_elem_lty = match *rv_pointee_lty.kind() {
                            TyKind::Array(..) | TyKind::Slice(..) => rv_pointee_lty.args[0],
                            TyKind::Str => self.acx.lcx().label(elem_ty, &mut |_| PointerId::NONE),
                            _ => unreachable!(),
                        };

                        // Map `pl_lty = *mut i32` to `pl_elem_lty = i32`
                        let pl_elem_lty = pl_lty.args[0];

                        self.do_unify(pl_elem_lty, rv_elem_lty);
                        self.do_assign_pointer_ids(pl_lty.label, rv_lty.label);
                    }

                    Callee::Malloc => {
                        self.visit_place(destination, Mutability::Mut);
                    }

                    Callee::Calloc => {
                        self.visit_place(destination, Mutability::Mut);
                    }
                    Callee::Realloc => {
                        self.visit_place(destination, Mutability::Mut);
                        let pl_lty = self.acx.type_of(destination);
                        assert!(args.len() == 2);
                        self.visit_operand(&args[0]);
                        let rv_lty = self.acx.type_of(&args[0]);

                        // input needs FREE permission
                        let perms = PermissionSet::FREE;
                        self.constraints.add_all_perms(rv_lty.label, perms);

                        // unify inner-most pointer types
                        self.do_equivalence_nested(pl_lty, rv_lty);
                    }
                    Callee::Free => {
                        self.visit_place(destination, Mutability::Mut);
                        assert!(args.len() == 1);
                        self.visit_operand(&args[0]);

                        let rv_lty = self.acx.type_of(&args[0]);
                        let perms = PermissionSet::FREE;
                        self.constraints.add_all_perms(rv_lty.label, perms);
                    }

                    Callee::IsNull => {
                        assert!(args.len() == 1);
                        self.visit_operand(&args[0]);
                    }

                    Callee::Other { def_id, substs } => {
                        self.visit_call_other(def_id, substs, args, destination);
                    }
                }
            }
            // TODO(spernsteiner): handle other `TerminatorKind`s
            _ => (),
        }
    }

    fn visit_call_other(
        &mut self,
        def_id: DefId,
        substs: SubstsRef<'tcx>,
        args: &[Operand<'tcx>],
        dest: Place<'tcx>,
    ) {
        let sig = match self.acx.gacx.fn_sigs.get(&def_id) {
            Some(&x) => x,
            None => todo!("call to unknown function {def_id:?}"),
        };
        if substs.non_erasable_generics().next().is_some() {
            todo!("call to generic function {def_id:?} {substs:?}");
        }

        // Process pseudo-assignments from `args` to the types declared in `sig`.
        for (arg_op, &input_lty) in args.iter().zip(sig.inputs.iter()) {
            self.visit_operand(arg_op);
            let arg_lty = self.acx.type_of(arg_op);
            self.do_assign(input_lty, arg_lty);
        }

        // Process a pseudo-assignment from the return type declared in `sig` to `dest`.
        self.visit_place(dest, Mutability::Mut);
        let dest_lty = self.acx.type_of(dest);
        let output_lty = sig.output;
        self.do_assign(dest_lty, output_lty);
    }
}

pub fn visit<'tcx>(
    acx: &AnalysisCtxt<'_, 'tcx>,
    mir: &Body<'tcx>,
) -> (DataflowConstraints, Vec<(PointerId, PointerId)>) {
    let mut tc = TypeChecker {
        acx,
        mir,
        constraints: DataflowConstraints::default(),
        equiv_constraints: Vec::new(),
    };

    for (bb, bb_data) in mir.basic_blocks().iter_enumerated() {
        for (i, stmt) in bb_data.statements.iter().enumerate() {
            tc.visit_statement(
                stmt,
                Location {
                    block: bb,
                    statement_index: i,
                },
            );
        }
        tc.visit_terminator(bb_data.terminator());
    }

    (tc.constraints, tc.equiv_constraints)
}
