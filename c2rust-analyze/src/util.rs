use crate::canonical_path::canonical_path;
use crate::labeled_ty::LabeledTy;
use crate::trivial::IsTrivial;
use rustc_hir::def::DefKind;
use rustc_hir::def_id::DefId;
use rustc_middle::mir::{
    Field, Local, Mutability, Operand, PlaceElem, PlaceRef, ProjectionElem, Rvalue,
};
use rustc_middle::ty::{self, AdtDef, DefIdTree, SubstsRef, Ty, TyCtxt, TyKind};
use std::fmt::Debug;

#[derive(Debug)]
pub enum RvalueDesc<'tcx> {
    /// A pointer projection, such as `&(*x.y).z`.  The rvalue is split into a base pointer
    /// expression (in this case `x.y`) and a projection (`.z`).  The `&` and `*` are implicit.
    Project {
        /// The base pointer of the projection.  This is guaranteed to evaluate to a pointer or
        /// reference type.
        ///
        /// This may contain derefs, indicating that the pointer was loaded through another
        /// pointer.  Only the outermost deref is implicit.  For example, `&(**x).y` has a `base`
        /// of `*x` and a `proj` of `.y`.
        base: PlaceRef<'tcx>,
        /// The projection applied to the pointer.  This contains no `Deref` projections.
        proj: &'tcx [PlaceElem<'tcx>],
    },
    /// The address of a local or one of its fields, such as `&x.y`.  The rvalue is split into a
    /// base local (in this case `x`) and a projection (`.y`).  The `&` is implicit.
    AddrOfLocal {
        local: Local,
        /// The projection applied to the local.  This contains no `Deref` projections.
        proj: &'tcx [PlaceElem<'tcx>],
    },
}

pub fn describe_rvalue<'tcx>(rv: &Rvalue<'tcx>) -> Option<RvalueDesc<'tcx>> {
    Some(match *rv {
        Rvalue::Use(ref op) => match *op {
            Operand::Move(pl) | Operand::Copy(pl) => RvalueDesc::Project {
                base: pl.as_ref(),
                proj: &[],
            },
            Operand::Constant(_) => return None,
        },
        Rvalue::Ref(_, _, pl) | Rvalue::AddressOf(_, pl) => {
            let projection = &pl.projection[..];
            match projection
                .iter()
                .rposition(|p| matches!(p, PlaceElem::Deref))
            {
                Some(i) => {
                    // `i` is the index of the last `ProjectionElem::Deref` in `pl`.
                    RvalueDesc::Project {
                        base: PlaceRef {
                            local: pl.local,
                            projection: &projection[..i],
                        },
                        proj: &projection[i + 1..],
                    }
                }
                None => {
                    // `pl` refers to a field/element of a local.
                    RvalueDesc::AddrOfLocal {
                        local: pl.local,
                        proj: projection,
                    }
                }
            }
        }
        _ => return None,
    })
}

#[derive(Debug)]
pub enum Callee<'tcx> {
    /// A [`Trivial`] library function is one that has no effect on pointer permissions in its caller.
    ///
    /// Thus, a [`Trivial`] function call requires no special handling.
    ///
    /// A function is [`Trivial`] if it has no argument or return types that are or contain a pointer.
    /// Note that "contains a pointer" is calculated recursively.
    /// There must not be any raw pointer accessible from that type.
    ///
    /// We ignore the possibility that a function may perform
    /// int-to-ptr casts (a la [`std::ptr::from_exposed_addr`]) internally,
    /// as handling such casts is very difficult and out of scope for now.
    ///
    /// References are allowed, because the existence of that reference in the first place
    /// carries much stronger semantics, so in the case that the reference is casted to a raw pointer,
    /// we can simply use the pointer permissions guaranteed by that reference.
    ///
    /// [`Trivial`]: Self::Trivial
    Trivial,

    /// A function whose definition is not known.
    ///
    /// This could be for multiple reasons.
    ///
    /// A function could be `extern`, so there is no source for it.
    /// Sometimes the actual definition could be linked with a `use` (the ideal solution),
    /// but sometimes it's completely external and thus completely unknown,
    /// as it may be dynamically linked.
    ///
    /// It could also be a function pointer,
    /// for which there could be multiple definitions.
    /// While possible definitions could be statically determined as an optimization,
    /// this provides a safe fallback.
    ///
    /// Or it could a function in another non-local crate, such as `std`,
    /// as definitions of functions from other crates are not available,
    /// and we definitely can't rewrite them at all.
    UnknownDef { ty: Ty<'tcx> },

    /// A function that:
    /// * is in the current, local crate
    /// * is statically-known
    /// * has an accessible definition
    /// * is non-trivial
    /// * is non-builtin
    LocalDef {
        def_id: DefId,
        substs: SubstsRef<'tcx>,
    },

    /// `<*mut T>::offset` or `<*const T>::offset`.
    PtrOffset {
        pointee_ty: Ty<'tcx>,
        mutbl: Mutability,
    },

    /// `<[T]>::as_ptr` and `<[T]>::as_mut_ptr` methods.  Also covers the array and str versions.
    SliceAsPtr {
        /// The pointee type.  This is either `TyKind::Slice`, `TyKind::Array`, or `TyKind::Str`.
        pointee_ty: Ty<'tcx>,

        /// The slice element type.  For `str`, this is `u8`.
        elem_ty: Ty<'tcx>,

        /// Mutability of the output pointer.
        mutbl: Mutability,
    },

    /// libc::malloc
    Malloc,

    /// libc::calloc
    Calloc,

    /// libc::free
    Free,

    /// libc::realloc
    Realloc,

    /// core::ptr::is_null
    IsNull,
}

pub fn ty_callee<'tcx>(tcx: TyCtxt<'tcx>, ty: Ty<'tcx>) -> Callee<'tcx> {
    let is_trivial = || {
        let is_trivial = ty.fn_sig(tcx).is_trivial(tcx);
        eprintln!("{ty:?} is trivial: {is_trivial}");
        is_trivial
    };

    match *ty.kind() {
        ty::FnDef(did, substs) => {
            if is_trivial() {
                return Callee::Trivial;
            }

            let name = canonical_path(tcx, ty);
            let parent_did = tcx.parent(did);
            let parent_impl_ty = || -> Ty<'tcx> { tcx.type_of(parent_did) };
            let inner_ty = |ty: Ty<'tcx>| -> Ty<'tcx> {
                match *ty.kind() {
                    ty::Array(ty, _) => ty,
                    ty::Slice(ty) => ty,
                    ty::RawPtr(tm) => tm.ty,
                    ty::Ref(_, ty, _) => ty,
                    _ => {
                        panic!("inner_ty called on {ty:?}, which is doesn't have a single inner Ty")
                    }
                }
            };

            match name.as_str() {
                "core::ptr::const_ptr::offset" => Callee::PtrOffset {
                    pointee_ty: inner_ty(parent_impl_ty()),
                    mutbl: Mutability::Not,
                },
                "core::ptr::mut_ptr::offset" => Callee::PtrOffset {
                    pointee_ty: inner_ty(parent_impl_ty()),
                    mutbl: Mutability::Mut,
                },

                "core::slice::as_ptr" => {
                    let pointee_ty = parent_impl_ty();
                    Callee::SliceAsPtr {
                        pointee_ty,
                        elem_ty: inner_ty(pointee_ty),
                        mutbl: Mutability::Not,
                    }
                }
                "core::slice::as_mut_ptr" => {
                    let pointee_ty = parent_impl_ty();
                    Callee::SliceAsPtr {
                        pointee_ty,
                        elem_ty: inner_ty(pointee_ty),
                        mutbl: Mutability::Mut,
                    }
                }
                "core::str::as_ptr" => Callee::SliceAsPtr {
                    pointee_ty: parent_impl_ty(),
                    elem_ty: tcx.types.u8,
                    mutbl: Mutability::Not,
                },
                "core::str::as_mut_ptr" => Callee::SliceAsPtr {
                    pointee_ty: parent_impl_ty(),
                    elem_ty: tcx.types.u8,
                    mutbl: Mutability::Mut,
                },

                "core::ptr::const_ptr::is_null" | "core::ptr::mut_ptr::is_null" => Callee::IsNull,

                "crate::{{extern}}::malloc" | "crate::{{extern}}::c2rust_test_typed_malloc" => {
                    Callee::Malloc
                }
                "crate::{{extern}}::calloc" | "crate::{{extern}}::c2rust_test_typed_calloc" => {
                    Callee::Calloc
                }
                "crate::{{extern}}::realloc" | "crate::{{extern}}::c2rust_test_typed_realloc" => {
                    Callee::Realloc
                }
                "crate::{{extern}}::free" | "crate::{{extern}}::c2rust_test_typed_free" => {
                    Callee::Free
                }

                _ => {
                    eprintln!("non-builtin: {name}");
                    if !did.is_local() || tcx.def_kind(parent_did) == DefKind::ForeignMod {
                        Callee::UnknownDef { ty }
                    } else {
                        Callee::LocalDef {
                            def_id: did,
                            substs,
                        }
                    }
                }
            }
        }
        ty::FnPtr(..) => {
            if is_trivial() {
                Callee::Trivial
            } else {
                Callee::UnknownDef { ty }
            }
        }
        _ => Callee::UnknownDef { ty },
    }
}

// fn builtin_callee<'tcx>(tcx: TyCtxt<'tcx>, ty: Ty<'tcx>, did: DefId) -> Option<Callee> {
//     let name = canonical_path(tcx, ty);

//     let parent_impl_ty = || -> Ty<'tcx> { tcx.type_of(tcx.parent(did)) };
//     let pointee_ty = || -> Ty<'tcx> {
//         let pointer_ty = parent_impl_ty();
//         match *pointer_ty.kind() {
//             ty::Array(ty, _) => ty,
//             ty::Slice(ty) => ty,
//             ty::RawPtr(tm) => tm.ty,
//             ty::Ref(_, ty, _) => ty,
//             _ => panic!("pointee_ty called on {pointer_ty:?}, which is not pointer-like"),
//         }
//     };

//     match name.as_str() {
//         "core::ptr::ptr::offset" => Some(Callee::PtrOffset {
//             pointee_ty: pointee_ty(),
//             mutbl: Mutability::Not,
//         }),
//         "core::ptr::mut_ptr::offset" => Some(Callee::PtrOffset {
//             pointee_ty: pointee_ty(),
//             mutbl: Mutability::Mut,
//         }),

//         "core::slice::as_ptr" => Some(Callee::SliceAsPtr {
//             pointee_ty: parent_impl_ty(),
//             elem_ty: pointee_ty(),
//             mutbl: Mutability::Not,
//         }),
//         "core::slice::as_mut_ptr" => Some(Callee::SliceAsPtr {
//             pointee_ty: parent_impl_ty(),
//             elem_ty: pointee_ty(),
//             mutbl: Mutability::Mut,
//         }),
//         "core::str::as_ptr" => Some(Callee::SliceAsPtr {
//             pointee_ty: parent_impl_ty(),
//             elem_ty: tcx.types.u8,
//             mutbl: Mutability::Not,
//         }),
//         "core::str::as_mut_ptr" => Some(Callee::SliceAsPtr {
//             pointee_ty: parent_impl_ty(),
//             elem_ty: tcx.types.u8,
//             mutbl: Mutability::Mut,
//         }),

//         "core::ptr::ptr::is_null" | "core::ptr::mut_ptr::is_null" => Some(Callee::IsNull),

//         "{{extern}}::malloc" | "{{extern}}::c2rust_test_typed_malloc" => Some(Callee::Malloc),
//         "{{extern}}::calloc" | "{{extern}}::c2rust_test_typed_calloc" => Some(Callee::Calloc),
//         "{{extern}}::realloc" | "{{extern}}::c2rust_test_typed_realloc" => Some(Callee::Realloc),
//         "{{extern}}::free" | "{{extern}}::c2rust_test_typed_free" => Some(Callee::Free),

//         _ => {
//             eprintln!("name: {name:?}");
//             None
//         }
//     }
// }

pub fn lty_project<'tcx, L: Debug>(
    lty: LabeledTy<'tcx, L>,
    proj: &PlaceElem<'tcx>,
    mut adt_func: impl FnMut(LabeledTy<'tcx, L>, AdtDef<'tcx>, Field) -> LabeledTy<'tcx, L>,
) -> LabeledTy<'tcx, L> {
    match *proj {
        ProjectionElem::Deref => {
            assert!(matches!(lty.kind(), TyKind::Ref(..) | TyKind::RawPtr(..)));
            assert_eq!(lty.args.len(), 1);
            lty.args[0]
        }
        ProjectionElem::Field(f, _) => match lty.kind() {
            TyKind::Tuple(_) => lty.args[f.index()],
            TyKind::Adt(def, _) => adt_func(lty, *def, f),
            _ => panic!("Field projection is unsupported on type {:?}", lty),
        },
        ProjectionElem::Index(..) | ProjectionElem::ConstantIndex { .. } => {
            assert!(matches!(lty.kind(), TyKind::Array(..) | TyKind::Slice(..)));
            assert_eq!(lty.args.len(), 1);
            lty.args[0]
        }
        ProjectionElem::Subslice { .. } => todo!("type_of Subslice"),
        ProjectionElem::Downcast(..) => todo!("type_of Downcast"),
    }
}
