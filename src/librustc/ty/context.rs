// Copyright 2012-2015 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! type context book-keeping

use dep_graph::{DepGraph, DepTrackingMap};
use hir::map as ast_map;
use session::Session;
use lint;
use middle;
use middle::cstore::LOCAL_CRATE;
use hir::def::DefMap;
use hir::def_id::DefId;
use middle::free_region::FreeRegionMap;
use middle::region::RegionMaps;
use middle::resolve_lifetime;
use middle::stability;
use ty::subst::{self, Subst, Substs};
use traits;
use ty::{self, TraitRef, Ty, TypeAndMut};
use ty::{TyS, TypeVariants};
use ty::{AdtDef, ClosureSubsts, ExistentialBounds, Region};
use hir::FreevarMap;
use ty::{BareFnTy, InferTy, ParamTy, ProjectionTy, TraitTy};
use ty::{TyVar, TyVid, IntVar, IntVid, FloatVar, FloatVid};
use ty::TypeVariants::*;
use ty::layout::{Layout, TargetDataLayout};
use ty::maps;
use util::common::MemoizationMap;
use util::nodemap::{NodeMap, NodeSet, DefIdMap, DefIdSet};
use util::nodemap::FnvHashMap;

use arena::TypedArena;
use std::borrow::Borrow;
use std::cell::{Cell, RefCell, Ref};
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use syntax::ast::{self, Name, NodeId};
use syntax::attr;
use syntax::parse::token::{self, keywords};

use hir;

/// Internal storage
pub struct CtxtArenas<'tcx> {
    // internings
    type_: TypedArena<TyS<'tcx>>,
    substs: TypedArena<Substs<'tcx>>,
    bare_fn: TypedArena<BareFnTy<'tcx>>,
    region: TypedArena<Region>,
    stability: TypedArena<attr::Stability>,
    layout: TypedArena<Layout>,

    // references
    trait_defs: TypedArena<ty::TraitDef<'tcx>>,
    adt_defs: TypedArena<ty::AdtDefData<'tcx, 'tcx>>,
}

impl<'tcx> CtxtArenas<'tcx> {
    pub fn new() -> CtxtArenas<'tcx> {
        CtxtArenas {
            type_: TypedArena::new(),
            substs: TypedArena::new(),
            bare_fn: TypedArena::new(),
            region: TypedArena::new(),
            stability: TypedArena::new(),
            layout: TypedArena::new(),

            trait_defs: TypedArena::new(),
            adt_defs: TypedArena::new()
        }
    }
}

pub struct CommonTypes<'tcx> {
    pub bool: Ty<'tcx>,
    pub char: Ty<'tcx>,
    pub isize: Ty<'tcx>,
    pub i8: Ty<'tcx>,
    pub i16: Ty<'tcx>,
    pub i32: Ty<'tcx>,
    pub i64: Ty<'tcx>,
    pub usize: Ty<'tcx>,
    pub u8: Ty<'tcx>,
    pub u16: Ty<'tcx>,
    pub u32: Ty<'tcx>,
    pub u64: Ty<'tcx>,
    pub f32: Ty<'tcx>,
    pub f64: Ty<'tcx>,
    pub err: Ty<'tcx>,
}

pub struct Tables<'tcx> {
    /// Stores the types for various nodes in the AST.  Note that this table
    /// is not guaranteed to be populated until after typeck.  See
    /// typeck::check::fn_ctxt for details.
    pub node_types: NodeMap<Ty<'tcx>>,

    /// Stores the type parameters which were substituted to obtain the type
    /// of this node.  This only applies to nodes that refer to entities
    /// parameterized by type parameters, such as generic fns, types, or
    /// other items.
    pub item_substs: NodeMap<ty::ItemSubsts<'tcx>>,

    pub adjustments: NodeMap<ty::adjustment::AutoAdjustment<'tcx>>,

    pub method_map: ty::MethodMap<'tcx>,

    /// Borrows
    pub upvar_capture_map: ty::UpvarCaptureMap,

    /// Records the type of each closure. The def ID is the ID of the
    /// expression defining the closure.
    pub closure_tys: DefIdMap<ty::ClosureTy<'tcx>>,

    /// Records the type of each closure. The def ID is the ID of the
    /// expression defining the closure.
    pub closure_kinds: DefIdMap<ty::ClosureKind>,

    /// For each fn, records the "liberated" types of its arguments
    /// and return type. Liberated means that all bound regions
    /// (including late-bound regions) are replaced with free
    /// equivalents. This table is not used in trans (since regions
    /// are erased there) and hence is not serialized to metadata.
    pub liberated_fn_sigs: NodeMap<ty::FnSig<'tcx>>,

    /// For each FRU expression, record the normalized types of the fields
    /// of the struct - this is needed because it is non-trivial to
    /// normalize while preserving regions. This table is used only in
    /// MIR construction and hence is not serialized to metadata.
    pub fru_field_types: NodeMap<Vec<Ty<'tcx>>>
}

impl<'tcx> Tables<'tcx> {
    pub fn empty() -> Tables<'tcx> {
        Tables {
            node_types: FnvHashMap(),
            item_substs: NodeMap(),
            adjustments: NodeMap(),
            method_map: FnvHashMap(),
            upvar_capture_map: FnvHashMap(),
            closure_tys: DefIdMap(),
            closure_kinds: DefIdMap(),
            liberated_fn_sigs: NodeMap(),
            fru_field_types: NodeMap()
        }
    }

    pub fn closure_kind(this: &RefCell<Self>,
                        tcx: &TyCtxt<'tcx>,
                        def_id: DefId)
                        -> ty::ClosureKind {
        // If this is a local def-id, it should be inserted into the
        // tables by typeck; else, it will be retreived from
        // the external crate metadata.
        if let Some(&kind) = this.borrow().closure_kinds.get(&def_id) {
            return kind;
        }

        let kind = tcx.sess.cstore.closure_kind(tcx, def_id);
        this.borrow_mut().closure_kinds.insert(def_id, kind);
        kind
    }

    pub fn closure_type(this: &RefCell<Self>,
                        tcx: &TyCtxt<'tcx>,
                        def_id: DefId,
                        substs: &ClosureSubsts<'tcx>)
                        -> ty::ClosureTy<'tcx>
    {
        // If this is a local def-id, it should be inserted into the
        // tables by typeck; else, it will be retreived from
        // the external crate metadata.
        if let Some(ty) = this.borrow().closure_tys.get(&def_id) {
            return ty.subst(tcx, &substs.func_substs);
        }

        let ty = tcx.sess.cstore.closure_ty(tcx, def_id);
        this.borrow_mut().closure_tys.insert(def_id, ty.clone());
        ty.subst(tcx, &substs.func_substs)
    }
}

impl<'tcx> CommonTypes<'tcx> {
    fn new(arena: &'tcx TypedArena<TyS<'tcx>>,
           interner: &RefCell<FnvHashMap<InternedTy<'tcx>, Ty<'tcx>>>)
           -> CommonTypes<'tcx>
    {
        let mk = |sty| TyCtxt::intern_ty(arena, interner, sty);
        CommonTypes {
            bool: mk(TyBool),
            char: mk(TyChar),
            err: mk(TyError),
            isize: mk(TyInt(ast::IntTy::Is)),
            i8: mk(TyInt(ast::IntTy::I8)),
            i16: mk(TyInt(ast::IntTy::I16)),
            i32: mk(TyInt(ast::IntTy::I32)),
            i64: mk(TyInt(ast::IntTy::I64)),
            usize: mk(TyUint(ast::UintTy::Us)),
            u8: mk(TyUint(ast::UintTy::U8)),
            u16: mk(TyUint(ast::UintTy::U16)),
            u32: mk(TyUint(ast::UintTy::U32)),
            u64: mk(TyUint(ast::UintTy::U64)),
            f32: mk(TyFloat(ast::FloatTy::F32)),
            f64: mk(TyFloat(ast::FloatTy::F64)),
        }
    }
}

/// The data structure to keep track of all the information that typechecker
/// generates so that so that it can be reused and doesn't have to be redone
/// later on.
pub struct TyCtxt<'tcx> {
    /// The arenas that types etc are allocated from.
    arenas: &'tcx CtxtArenas<'tcx>,

    /// Specifically use a speedy hash algorithm for this hash map, it's used
    /// quite often.
    // FIXME(eddyb) use a FnvHashSet<InternedTy<'tcx>> when equivalent keys can
    // queried from a HashSet.
    interner: RefCell<FnvHashMap<InternedTy<'tcx>, Ty<'tcx>>>,

    // FIXME as above, use a hashset if equivalent elements can be queried.
    substs_interner: RefCell<FnvHashMap<&'tcx Substs<'tcx>, &'tcx Substs<'tcx>>>,
    bare_fn_interner: RefCell<FnvHashMap<&'tcx BareFnTy<'tcx>, &'tcx BareFnTy<'tcx>>>,
    region_interner: RefCell<FnvHashMap<&'tcx Region, &'tcx Region>>,
    stability_interner: RefCell<FnvHashMap<&'tcx attr::Stability, &'tcx attr::Stability>>,
    layout_interner: RefCell<FnvHashMap<&'tcx Layout, &'tcx Layout>>,

    pub dep_graph: DepGraph,

    /// Common types, pre-interned for your convenience.
    pub types: CommonTypes<'tcx>,

    pub sess: &'tcx Session,
    pub def_map: RefCell<DefMap>,

    pub named_region_map: resolve_lifetime::NamedRegionMap,

    pub region_maps: RegionMaps,

    // For each fn declared in the local crate, type check stores the
    // free-region relationships that were deduced from its where
    // clauses and parameter types. These are then read-again by
    // borrowck. (They are not used during trans, and hence are not
    // serialized or needed for cross-crate fns.)
    free_region_maps: RefCell<NodeMap<FreeRegionMap>>,
    // FIXME: jroesch make this a refcell

    pub tables: RefCell<Tables<'tcx>>,

    /// Maps from a trait item to the trait item "descriptor"
    pub impl_or_trait_items: RefCell<DepTrackingMap<maps::ImplOrTraitItems<'tcx>>>,

    /// Maps from a trait def-id to a list of the def-ids of its trait items
    pub trait_item_def_ids: RefCell<DepTrackingMap<maps::TraitItemDefIds<'tcx>>>,

    /// A cache for the trait_items() routine; note that the routine
    /// itself pushes the `TraitItems` dependency node.
    trait_items_cache: RefCell<DepTrackingMap<maps::TraitItems<'tcx>>>,

    pub impl_trait_refs: RefCell<DepTrackingMap<maps::ImplTraitRefs<'tcx>>>,
    pub trait_defs: RefCell<DepTrackingMap<maps::TraitDefs<'tcx>>>,
    pub adt_defs: RefCell<DepTrackingMap<maps::AdtDefs<'tcx>>>,

    /// Maps from the def-id of an item (trait/struct/enum/fn) to its
    /// associated predicates.
    pub predicates: RefCell<DepTrackingMap<maps::Predicates<'tcx>>>,

    /// Maps from the def-id of a trait to the list of
    /// super-predicates. This is a subset of the full list of
    /// predicates. We store these in a separate map because we must
    /// evaluate them even during type conversion, often before the
    /// full predicates are available (note that supertraits have
    /// additional acyclicity requirements).
    pub super_predicates: RefCell<DepTrackingMap<maps::Predicates<'tcx>>>,

    pub map: ast_map::Map<'tcx>,

    // Records the free variables refrenced by every closure
    // expression. Do not track deps for this, just recompute it from
    // scratch every time.
    pub freevars: RefCell<FreevarMap>,

    // Records the type of every item.
    pub tcache: RefCell<DepTrackingMap<maps::Tcache<'tcx>>>,

    // Internal cache for metadata decoding. No need to track deps on this.
    pub rcache: RefCell<FnvHashMap<ty::CReaderCacheKey, Ty<'tcx>>>,

    // Cache for the type-contents routine. FIXME -- track deps?
    pub tc_cache: RefCell<FnvHashMap<Ty<'tcx>, ty::contents::TypeContents>>,

    // Cache for various types within a method body and so forth.
    //
    // FIXME this should be made local to typeck, but it is currently used by one lint
    pub ast_ty_to_ty_cache: RefCell<NodeMap<Ty<'tcx>>>,

    // FIXME no dep tracking, but we should be able to remove this
    pub ty_param_defs: RefCell<NodeMap<ty::TypeParameterDef<'tcx>>>,

    // FIXME dep tracking -- should be harmless enough
    pub normalized_cache: RefCell<FnvHashMap<Ty<'tcx>, Ty<'tcx>>>,

    pub lang_items: middle::lang_items::LanguageItems,

    /// Maps from def-id of a type or region parameter to its
    /// (inferred) variance.
    pub item_variance_map: RefCell<DepTrackingMap<maps::ItemVariances<'tcx>>>,

    /// True if the variance has been computed yet; false otherwise.
    pub variance_computed: Cell<bool>,

    /// Maps a DefId of a type to a list of its inherent impls.
    /// Contains implementations of methods that are inherent to a type.
    /// Methods in these implementations don't need to be exported.
    pub inherent_impls: RefCell<DepTrackingMap<maps::InherentImpls<'tcx>>>,

    /// Maps a DefId of an impl to a list of its items.
    /// Note that this contains all of the impls that we know about,
    /// including ones in other crates. It's not clear that this is the best
    /// way to do it.
    pub impl_items: RefCell<DepTrackingMap<maps::ImplItems<'tcx>>>,

    /// Set of used unsafe nodes (functions or blocks). Unsafe nodes not
    /// present in this set can be warned about.
    pub used_unsafe: RefCell<NodeSet>,

    /// Set of nodes which mark locals as mutable which end up getting used at
    /// some point. Local variable definitions not in this set can be warned
    /// about.
    pub used_mut_nodes: RefCell<NodeSet>,

    /// The set of external nominal types whose implementations have been read.
    /// This is used for lazy resolution of methods.
    pub populated_external_types: RefCell<DefIdSet>,

    /// The set of external primitive types whose implementations have been read.
    /// FIXME(arielb1): why is this separate from populated_external_types?
    pub populated_external_primitive_impls: RefCell<DefIdSet>,

    /// Cache used by const_eval when decoding external constants.
    /// Contains `None` when the constant has been fetched but doesn't exist.
    /// Constains `Some(expr_id, type)` otherwise.
    /// `type` is `None` in case it's not a primitive type
    pub extern_const_statics: RefCell<DefIdMap<Option<(NodeId, Option<Ty<'tcx>>)>>>,
    /// Cache used by const_eval when decoding extern const fns
    pub extern_const_fns: RefCell<DefIdMap<NodeId>>,

    pub node_lint_levels: RefCell<FnvHashMap<(NodeId, lint::LintId),
                                              lint::LevelSource>>,

    /// Maps any item's def-id to its stability index.
    pub stability: RefCell<stability::Index<'tcx>>,

    /// Caches the results of trait selection. This cache is used
    /// for things that do not have to do with the parameters in scope.
    pub selection_cache: traits::SelectionCache<'tcx>,

    /// Caches the results of trait evaluation. This cache is used
    /// for things that do not have to do with the parameters in scope.
    /// Merge this with `selection_cache`?
    pub evaluation_cache: traits::EvaluationCache<'tcx>,

    /// A set of predicates that have been fulfilled *somewhere*.
    /// This is used to avoid duplicate work. Predicates are only
    /// added to this set when they mention only "global" names
    /// (i.e., no type or lifetime parameters).
    pub fulfilled_predicates: RefCell<traits::GlobalFulfilledPredicates<'tcx>>,

    /// Caches the representation hints for struct definitions.
    repr_hint_cache: RefCell<DepTrackingMap<maps::ReprHints<'tcx>>>,

    /// Maps Expr NodeId's to their constant qualification.
    pub const_qualif_map: RefCell<NodeMap<middle::const_qualif::ConstQualif>>,

    /// Caches CoerceUnsized kinds for impls on custom types.
    pub custom_coerce_unsized_kinds: RefCell<DefIdMap<ty::adjustment::CustomCoerceUnsized>>,

    /// Maps a cast expression to its kind. This is keyed on the
    /// *from* expression of the cast, not the cast itself.
    pub cast_kinds: RefCell<NodeMap<ty::cast::CastKind>>,

    /// Maps Fn items to a collection of fragment infos.
    ///
    /// The main goal is to identify data (each of which may be moved
    /// or assigned) whose subparts are not moved nor assigned
    /// (i.e. their state is *unfragmented*) and corresponding ast
    /// nodes where the path to that data is moved or assigned.
    ///
    /// In the long term, unfragmented values will have their
    /// destructor entirely driven by a single stack-local drop-flag,
    /// and their parents, the collections of the unfragmented values
    /// (or more simply, "fragmented values"), are mapped to the
    /// corresponding collections of stack-local drop-flags.
    ///
    /// (However, in the short term that is not the case; e.g. some
    /// unfragmented paths still need to be zeroed, namely when they
    /// reference parent data from an outer scope that was not
    /// entirely moved, and therefore that needs to be zeroed so that
    /// we do not get double-drop when we hit the end of the parent
    /// scope.)
    ///
    /// Also: currently the table solely holds keys for node-ids of
    /// unfragmented values (see `FragmentInfo` enum definition), but
    /// longer-term we will need to also store mappings from
    /// fragmented data to the set of unfragmented pieces that
    /// constitute it.
    pub fragment_infos: RefCell<DefIdMap<Vec<ty::FragmentInfo>>>,

    /// The definite name of the current crate after taking into account
    /// attributes, commandline parameters, etc.
    pub crate_name: token::InternedString,

    /// Data layout specification for the current target.
    pub data_layout: TargetDataLayout,

    /// Cache for layouts computed from types.
    pub layout_cache: RefCell<FnvHashMap<Ty<'tcx>, &'tcx Layout>>,
}

impl<'tcx> TyCtxt<'tcx> {
    pub fn crate_name(&self, cnum: ast::CrateNum) -> token::InternedString {
        if cnum == LOCAL_CRATE {
            self.crate_name.clone()
        } else {
            self.sess.cstore.crate_name(cnum)
        }
    }

    pub fn crate_disambiguator(&self, cnum: ast::CrateNum) -> token::InternedString {
        if cnum == LOCAL_CRATE {
            self.sess.crate_disambiguator.get().as_str()
        } else {
            self.sess.cstore.crate_disambiguator(cnum)
        }
    }

    pub fn type_parameter_def(&self,
                              node_id: NodeId)
                              -> ty::TypeParameterDef<'tcx>
    {
        self.ty_param_defs.borrow().get(&node_id).unwrap().clone()
    }

    pub fn node_types(&self) -> Ref<NodeMap<Ty<'tcx>>> {
        fn projection<'a, 'tcx>(tables: &'a Tables<'tcx>) -> &'a NodeMap<Ty<'tcx>> {
            &tables.node_types
        }

        Ref::map(self.tables.borrow(), projection)
    }

    pub fn node_type_insert(&self, id: NodeId, ty: Ty<'tcx>) {
        self.tables.borrow_mut().node_types.insert(id, ty);
    }

    pub fn intern_trait_def(&self, def: ty::TraitDef<'tcx>)
                            -> &'tcx ty::TraitDef<'tcx> {
        let did = def.trait_ref.def_id;
        let interned = self.arenas.trait_defs.alloc(def);
        if let Some(prev) = self.trait_defs.borrow_mut().insert(did, interned) {
            bug!("Tried to overwrite interned TraitDef: {:?}", prev)
        }
        interned
    }

    pub fn alloc_trait_def(&self, def: ty::TraitDef<'tcx>)
                           -> &'tcx ty::TraitDef<'tcx> {
        self.arenas.trait_defs.alloc(def)
    }

    pub fn intern_adt_def(&self,
                          did: DefId,
                          kind: ty::AdtKind,
                          variants: Vec<ty::VariantDefData<'tcx, 'tcx>>)
                          -> ty::AdtDefMaster<'tcx> {
        let def = ty::AdtDefData::new(self, did, kind, variants);
        let interned = self.arenas.adt_defs.alloc(def);
        // this will need a transmute when reverse-variance is removed
        if let Some(prev) = self.adt_defs.borrow_mut().insert(did, interned) {
            bug!("Tried to overwrite interned AdtDef: {:?}", prev)
        }
        interned
    }

    pub fn intern_stability(&self, stab: attr::Stability) -> &'tcx attr::Stability {
        if let Some(st) = self.stability_interner.borrow().get(&stab) {
            return st;
        }

        let interned = self.arenas.stability.alloc(stab);
        if let Some(prev) = self.stability_interner
                                .borrow_mut()
                                .insert(interned, interned) {
            bug!("Tried to overwrite interned Stability: {:?}", prev)
        }
        interned
    }

    pub fn intern_layout(&self, layout: Layout) -> &'tcx Layout {
        if let Some(layout) = self.layout_interner.borrow().get(&layout) {
            return layout;
        }

        let interned = self.arenas.layout.alloc(layout);
        if let Some(prev) = self.layout_interner
                                .borrow_mut()
                                .insert(interned, interned) {
            bug!("Tried to overwrite interned Layout: {:?}", prev)
        }
        interned
    }

    pub fn store_free_region_map(&self, id: NodeId, map: FreeRegionMap) {
        if self.free_region_maps.borrow_mut().insert(id, map).is_some() {
            bug!("Tried to overwrite interned FreeRegionMap for NodeId {:?}", id)
        }
    }

    pub fn free_region_map(&self, id: NodeId) -> FreeRegionMap {
        self.free_region_maps.borrow()[&id].clone()
    }

    pub fn lift<T: ?Sized + Lift<'tcx>>(&self, value: &T) -> Option<T::Lifted> {
        value.lift_to_tcx(self)
    }

    /// Create a type context and call the closure with a `&TyCtxt` reference
    /// to the context. The closure enforces that the type context and any interned
    /// value (types, substs, etc.) can only be used while `ty::tls` has a valid
    /// reference to the context, to allow formatting values that need it.
    pub fn create_and_enter<F, R>(s: &'tcx Session,
                                 arenas: &'tcx CtxtArenas<'tcx>,
                                 def_map: RefCell<DefMap>,
                                 named_region_map: resolve_lifetime::NamedRegionMap,
                                 map: ast_map::Map<'tcx>,
                                 freevars: FreevarMap,
                                 region_maps: RegionMaps,
                                 lang_items: middle::lang_items::LanguageItems,
                                 stability: stability::Index<'tcx>,
                                 crate_name: &str,
                                 f: F) -> R
                                 where F: FnOnce(&TyCtxt<'tcx>) -> R
    {
        let data_layout = TargetDataLayout::parse(s);
        let interner = RefCell::new(FnvHashMap());
        let common_types = CommonTypes::new(&arenas.type_, &interner);
        let dep_graph = map.dep_graph.clone();
        let fulfilled_predicates = traits::GlobalFulfilledPredicates::new(dep_graph.clone());
        tls::enter(TyCtxt {
            arenas: arenas,
            interner: interner,
            substs_interner: RefCell::new(FnvHashMap()),
            bare_fn_interner: RefCell::new(FnvHashMap()),
            region_interner: RefCell::new(FnvHashMap()),
            stability_interner: RefCell::new(FnvHashMap()),
            layout_interner: RefCell::new(FnvHashMap()),
            dep_graph: dep_graph.clone(),
            types: common_types,
            named_region_map: named_region_map,
            region_maps: region_maps,
            free_region_maps: RefCell::new(FnvHashMap()),
            item_variance_map: RefCell::new(DepTrackingMap::new(dep_graph.clone())),
            variance_computed: Cell::new(false),
            sess: s,
            def_map: def_map,
            tables: RefCell::new(Tables::empty()),
            impl_trait_refs: RefCell::new(DepTrackingMap::new(dep_graph.clone())),
            trait_defs: RefCell::new(DepTrackingMap::new(dep_graph.clone())),
            adt_defs: RefCell::new(DepTrackingMap::new(dep_graph.clone())),
            predicates: RefCell::new(DepTrackingMap::new(dep_graph.clone())),
            super_predicates: RefCell::new(DepTrackingMap::new(dep_graph.clone())),
            fulfilled_predicates: RefCell::new(fulfilled_predicates),
            map: map,
            freevars: RefCell::new(freevars),
            tcache: RefCell::new(DepTrackingMap::new(dep_graph.clone())),
            rcache: RefCell::new(FnvHashMap()),
            tc_cache: RefCell::new(FnvHashMap()),
            ast_ty_to_ty_cache: RefCell::new(NodeMap()),
            impl_or_trait_items: RefCell::new(DepTrackingMap::new(dep_graph.clone())),
            trait_item_def_ids: RefCell::new(DepTrackingMap::new(dep_graph.clone())),
            trait_items_cache: RefCell::new(DepTrackingMap::new(dep_graph.clone())),
            ty_param_defs: RefCell::new(NodeMap()),
            normalized_cache: RefCell::new(FnvHashMap()),
            lang_items: lang_items,
            inherent_impls: RefCell::new(DepTrackingMap::new(dep_graph.clone())),
            impl_items: RefCell::new(DepTrackingMap::new(dep_graph.clone())),
            used_unsafe: RefCell::new(NodeSet()),
            used_mut_nodes: RefCell::new(NodeSet()),
            populated_external_types: RefCell::new(DefIdSet()),
            populated_external_primitive_impls: RefCell::new(DefIdSet()),
            extern_const_statics: RefCell::new(DefIdMap()),
            extern_const_fns: RefCell::new(DefIdMap()),
            node_lint_levels: RefCell::new(FnvHashMap()),
            stability: RefCell::new(stability),
            selection_cache: traits::SelectionCache::new(),
            evaluation_cache: traits::EvaluationCache::new(),
            repr_hint_cache: RefCell::new(DepTrackingMap::new(dep_graph.clone())),
            const_qualif_map: RefCell::new(NodeMap()),
            custom_coerce_unsized_kinds: RefCell::new(DefIdMap()),
            cast_kinds: RefCell::new(NodeMap()),
            fragment_infos: RefCell::new(DefIdMap()),
            crate_name: token::intern_and_get_ident(crate_name),
            data_layout: data_layout,
            layout_cache: RefCell::new(FnvHashMap()),
       }, f)
    }
}

/// A trait implemented for all X<'a> types which can be safely and
/// efficiently converted to X<'tcx> as long as they are part of the
/// provided TyCtxt<'tcx>.
/// This can be done, for example, for Ty<'tcx> or &'tcx Substs<'tcx>
/// by looking them up in their respective interners.
/// None is returned if the value or one of the components is not part
/// of the provided context.
/// For Ty, None can be returned if either the type interner doesn't
/// contain the TypeVariants key or if the address of the interned
/// pointer differs. The latter case is possible if a primitive type,
/// e.g. `()` or `u8`, was interned in a different context.
pub trait Lift<'tcx> {
    type Lifted;
    fn lift_to_tcx(&self, tcx: &TyCtxt<'tcx>) -> Option<Self::Lifted>;
}

impl<'a, 'tcx> Lift<'tcx> for Ty<'a> {
    type Lifted = Ty<'tcx>;
    fn lift_to_tcx(&self, tcx: &TyCtxt<'tcx>) -> Option<Ty<'tcx>> {
        if let Some(&ty) = tcx.interner.borrow().get(&self.sty) {
            if *self as *const _ == ty as *const _ {
                return Some(ty);
            }
        }
        None
    }
}

impl<'a, 'tcx> Lift<'tcx> for &'a Substs<'a> {
    type Lifted = &'tcx Substs<'tcx>;
    fn lift_to_tcx(&self, tcx: &TyCtxt<'tcx>) -> Option<&'tcx Substs<'tcx>> {
        if let Some(&substs) = tcx.substs_interner.borrow().get(*self) {
            if *self as *const _ == substs as *const _ {
                return Some(substs);
            }
        }
        None
    }
}


pub mod tls {
    use ty::TyCtxt;

    use std::cell::Cell;
    use std::fmt;
    use syntax::codemap;

    /// Marker type used for the scoped TLS slot.
    /// The type context cannot be used directly because the scoped TLS
    /// in libstd doesn't allow types generic over lifetimes.
    struct ThreadLocalTyCx;

    thread_local! {
        static TLS_TCX: Cell<Option<*const ThreadLocalTyCx>> = Cell::new(None)
    }

    fn span_debug(span: codemap::Span, f: &mut fmt::Formatter) -> fmt::Result {
        with(|tcx| {
            write!(f, "{}", tcx.sess.codemap().span_to_string(span))
        })
    }

    pub fn enter<'tcx, F: FnOnce(&TyCtxt<'tcx>) -> R, R>(tcx: TyCtxt<'tcx>, f: F) -> R {
        codemap::SPAN_DEBUG.with(|span_dbg| {
            let original_span_debug = span_dbg.get();
            span_dbg.set(span_debug);
            let tls_ptr = &tcx as *const _ as *const ThreadLocalTyCx;
            let result = TLS_TCX.with(|tls| {
                let prev = tls.get();
                tls.set(Some(tls_ptr));
                let ret = f(&tcx);
                tls.set(prev);
                ret
            });
            span_dbg.set(original_span_debug);
            result
        })
    }

    pub fn with<F: FnOnce(&TyCtxt) -> R, R>(f: F) -> R {
        TLS_TCX.with(|tcx| {
            let tcx = tcx.get().unwrap();
            f(unsafe { &*(tcx as *const TyCtxt) })
        })
    }

    pub fn with_opt<F: FnOnce(Option<&TyCtxt>) -> R, R>(f: F) -> R {
        if TLS_TCX.with(|tcx| tcx.get().is_some()) {
            with(|v| f(Some(v)))
        } else {
            f(None)
        }
    }
}

macro_rules! sty_debug_print {
    ($ctxt: expr, $($variant: ident),*) => {{
        // curious inner module to allow variant names to be used as
        // variable names.
        #[allow(non_snake_case)]
        mod inner {
            use ty::{self, TyCtxt};
            #[derive(Copy, Clone)]
            struct DebugStat {
                total: usize,
                region_infer: usize,
                ty_infer: usize,
                both_infer: usize,
            }

            pub fn go(tcx: &TyCtxt) {
                let mut total = DebugStat {
                    total: 0,
                    region_infer: 0, ty_infer: 0, both_infer: 0,
                };
                $(let mut $variant = total;)*


                for (_, t) in tcx.interner.borrow().iter() {
                    let variant = match t.sty {
                        ty::TyBool | ty::TyChar | ty::TyInt(..) | ty::TyUint(..) |
                            ty::TyFloat(..) | ty::TyStr => continue,
                        ty::TyError => /* unimportant */ continue,
                        $(ty::$variant(..) => &mut $variant,)*
                    };
                    let region = t.flags.get().intersects(ty::TypeFlags::HAS_RE_INFER);
                    let ty = t.flags.get().intersects(ty::TypeFlags::HAS_TY_INFER);

                    variant.total += 1;
                    total.total += 1;
                    if region { total.region_infer += 1; variant.region_infer += 1 }
                    if ty { total.ty_infer += 1; variant.ty_infer += 1 }
                    if region && ty { total.both_infer += 1; variant.both_infer += 1 }
                }
                println!("Ty interner             total           ty region  both");
                $(println!("    {:18}: {uses:6} {usespc:4.1}%, \
{ty:4.1}% {region:5.1}% {both:4.1}%",
                           stringify!($variant),
                           uses = $variant.total,
                           usespc = $variant.total as f64 * 100.0 / total.total as f64,
                           ty = $variant.ty_infer as f64 * 100.0  / total.total as f64,
                           region = $variant.region_infer as f64 * 100.0  / total.total as f64,
                           both = $variant.both_infer as f64 * 100.0  / total.total as f64);
                  )*
                println!("                  total {uses:6}        \
{ty:4.1}% {region:5.1}% {both:4.1}%",
                         uses = total.total,
                         ty = total.ty_infer as f64 * 100.0  / total.total as f64,
                         region = total.region_infer as f64 * 100.0  / total.total as f64,
                         both = total.both_infer as f64 * 100.0  / total.total as f64)
            }
        }

        inner::go($ctxt)
    }}
}

impl<'tcx> TyCtxt<'tcx> {
    pub fn print_debug_stats(&self) {
        sty_debug_print!(
            self,
            TyEnum, TyBox, TyArray, TySlice, TyRawPtr, TyRef, TyFnDef, TyFnPtr,
            TyTrait, TyStruct, TyClosure, TyTuple, TyParam, TyInfer, TyProjection);

        println!("Substs interner: #{}", self.substs_interner.borrow().len());
        println!("BareFnTy interner: #{}", self.bare_fn_interner.borrow().len());
        println!("Region interner: #{}", self.region_interner.borrow().len());
        println!("Stability interner: #{}", self.stability_interner.borrow().len());
        println!("Layout interner: #{}", self.layout_interner.borrow().len());
    }
}


/// An entry in the type interner.
pub struct InternedTy<'tcx> {
    ty: Ty<'tcx>
}

// NB: An InternedTy compares and hashes as a sty.
impl<'tcx> PartialEq for InternedTy<'tcx> {
    fn eq(&self, other: &InternedTy<'tcx>) -> bool {
        self.ty.sty == other.ty.sty
    }
}

impl<'tcx> Eq for InternedTy<'tcx> {}

impl<'tcx> Hash for InternedTy<'tcx> {
    fn hash<H: Hasher>(&self, s: &mut H) {
        self.ty.sty.hash(s)
    }
}

impl<'tcx> Borrow<TypeVariants<'tcx>> for InternedTy<'tcx> {
    fn borrow<'a>(&'a self) -> &'a TypeVariants<'tcx> {
        &self.ty.sty
    }
}

fn bound_list_is_sorted(bounds: &[ty::PolyProjectionPredicate]) -> bool {
    bounds.is_empty() ||
        bounds[1..].iter().enumerate().all(
            |(index, bound)| bounds[index].sort_key() <= bound.sort_key())
}

impl<'tcx> TyCtxt<'tcx> {
    // Type constructors
    pub fn mk_substs(&self, substs: Substs<'tcx>) -> &'tcx Substs<'tcx> {
        if let Some(substs) = self.substs_interner.borrow().get(&substs) {
            return *substs;
        }

        let substs = self.arenas.substs.alloc(substs);
        self.substs_interner.borrow_mut().insert(substs, substs);
        substs
    }

    /// Create an unsafe fn ty based on a safe fn ty.
    pub fn safe_to_unsafe_fn_ty(&self, bare_fn: &BareFnTy<'tcx>) -> Ty<'tcx> {
        assert_eq!(bare_fn.unsafety, hir::Unsafety::Normal);
        self.mk_fn_ptr(ty::BareFnTy {
            unsafety: hir::Unsafety::Unsafe,
            abi: bare_fn.abi,
            sig: bare_fn.sig.clone()
        })
    }

    pub fn mk_bare_fn(&self, bare_fn: BareFnTy<'tcx>) -> &'tcx BareFnTy<'tcx> {
        if let Some(bare_fn) = self.bare_fn_interner.borrow().get(&bare_fn) {
            return *bare_fn;
        }

        let bare_fn = self.arenas.bare_fn.alloc(bare_fn);
        self.bare_fn_interner.borrow_mut().insert(bare_fn, bare_fn);
        bare_fn
    }

    pub fn mk_region(&self, region: Region) -> &'tcx Region {
        if let Some(region) = self.region_interner.borrow().get(&region) {
            return *region;
        }

        let region = self.arenas.region.alloc(region);
        self.region_interner.borrow_mut().insert(region, region);
        region
    }

    fn intern_ty(type_arena: &'tcx TypedArena<TyS<'tcx>>,
                 interner: &RefCell<FnvHashMap<InternedTy<'tcx>, Ty<'tcx>>>,
                 st: TypeVariants<'tcx>)
                 -> Ty<'tcx> {
        let ty: Ty /* don't be &mut TyS */ = {
            let mut interner = interner.borrow_mut();
            match interner.get(&st) {
                Some(ty) => return *ty,
                _ => ()
            }

            let flags = super::flags::FlagComputation::for_sty(&st);

            let ty = match () {
                () => type_arena.alloc(TyS { sty: st,
                                             flags: Cell::new(flags.flags),
                                             region_depth: flags.depth, }),
            };

            interner.insert(InternedTy { ty: ty }, ty);
            ty
        };

        debug!("Interned type: {:?} Pointer: {:?}",
            ty, ty as *const TyS);
        ty
    }

    // Interns a type/name combination, stores the resulting box in cx.interner,
    // and returns the box as cast to an unsafe ptr (see comments for Ty above).
    pub fn mk_ty(&self, st: TypeVariants<'tcx>) -> Ty<'tcx> {
        TyCtxt::intern_ty(&self.arenas.type_, &self.interner, st)
    }

    pub fn mk_mach_int(&self, tm: ast::IntTy) -> Ty<'tcx> {
        match tm {
            ast::IntTy::Is   => self.types.isize,
            ast::IntTy::I8   => self.types.i8,
            ast::IntTy::I16  => self.types.i16,
            ast::IntTy::I32  => self.types.i32,
            ast::IntTy::I64  => self.types.i64,
        }
    }

    pub fn mk_mach_uint(&self, tm: ast::UintTy) -> Ty<'tcx> {
        match tm {
            ast::UintTy::Us   => self.types.usize,
            ast::UintTy::U8   => self.types.u8,
            ast::UintTy::U16  => self.types.u16,
            ast::UintTy::U32  => self.types.u32,
            ast::UintTy::U64  => self.types.u64,
        }
    }

    pub fn mk_mach_float(&self, tm: ast::FloatTy) -> Ty<'tcx> {
        match tm {
            ast::FloatTy::F32  => self.types.f32,
            ast::FloatTy::F64  => self.types.f64,
        }
    }

    pub fn mk_str(&self) -> Ty<'tcx> {
        self.mk_ty(TyStr)
    }

    pub fn mk_static_str(&self) -> Ty<'tcx> {
        self.mk_imm_ref(self.mk_region(ty::ReStatic), self.mk_str())
    }

    pub fn mk_enum(&self, def: AdtDef<'tcx>, substs: &'tcx Substs<'tcx>) -> Ty<'tcx> {
        // take a copy of substs so that we own the vectors inside
        self.mk_ty(TyEnum(def, substs))
    }

    pub fn mk_box(&self, ty: Ty<'tcx>) -> Ty<'tcx> {
        self.mk_ty(TyBox(ty))
    }

    pub fn mk_ptr(&self, tm: TypeAndMut<'tcx>) -> Ty<'tcx> {
        self.mk_ty(TyRawPtr(tm))
    }

    pub fn mk_ref(&self, r: &'tcx Region, tm: TypeAndMut<'tcx>) -> Ty<'tcx> {
        self.mk_ty(TyRef(r, tm))
    }

    pub fn mk_mut_ref(&self, r: &'tcx Region, ty: Ty<'tcx>) -> Ty<'tcx> {
        self.mk_ref(r, TypeAndMut {ty: ty, mutbl: hir::MutMutable})
    }

    pub fn mk_imm_ref(&self, r: &'tcx Region, ty: Ty<'tcx>) -> Ty<'tcx> {
        self.mk_ref(r, TypeAndMut {ty: ty, mutbl: hir::MutImmutable})
    }

    pub fn mk_mut_ptr(&self, ty: Ty<'tcx>) -> Ty<'tcx> {
        self.mk_ptr(TypeAndMut {ty: ty, mutbl: hir::MutMutable})
    }

    pub fn mk_imm_ptr(&self, ty: Ty<'tcx>) -> Ty<'tcx> {
        self.mk_ptr(TypeAndMut {ty: ty, mutbl: hir::MutImmutable})
    }

    pub fn mk_nil_ptr(&self) -> Ty<'tcx> {
        self.mk_imm_ptr(self.mk_nil())
    }

    pub fn mk_array(&self, ty: Ty<'tcx>, n: usize) -> Ty<'tcx> {
        self.mk_ty(TyArray(ty, n))
    }

    pub fn mk_slice(&self, ty: Ty<'tcx>) -> Ty<'tcx> {
        self.mk_ty(TySlice(ty))
    }

    pub fn mk_tup(&self, ts: Vec<Ty<'tcx>>) -> Ty<'tcx> {
        self.mk_ty(TyTuple(ts))
    }

    pub fn mk_nil(&self) -> Ty<'tcx> {
        self.mk_tup(Vec::new())
    }

    pub fn mk_bool(&self) -> Ty<'tcx> {
        self.mk_ty(TyBool)
    }

    pub fn mk_fn_def(&self, def_id: DefId,
                     substs: &'tcx Substs<'tcx>,
                     fty: BareFnTy<'tcx>) -> Ty<'tcx> {
        self.mk_ty(TyFnDef(def_id, substs, self.mk_bare_fn(fty)))
    }

    pub fn mk_fn_ptr(&self, fty: BareFnTy<'tcx>) -> Ty<'tcx> {
        self.mk_ty(TyFnPtr(self.mk_bare_fn(fty)))
    }

    pub fn mk_trait(&self,
                    principal: ty::PolyTraitRef<'tcx>,
                    bounds: ExistentialBounds<'tcx>)
                    -> Ty<'tcx>
    {
        assert!(bound_list_is_sorted(&bounds.projection_bounds));

        let inner = box TraitTy {
            principal: principal,
            bounds: bounds
        };
        self.mk_ty(TyTrait(inner))
    }

    pub fn mk_projection(&self,
                         trait_ref: TraitRef<'tcx>,
                         item_name: Name)
                         -> Ty<'tcx> {
        // take a copy of substs so that we own the vectors inside
        let inner = ProjectionTy { trait_ref: trait_ref, item_name: item_name };
        self.mk_ty(TyProjection(inner))
    }

    pub fn mk_struct(&self, def: AdtDef<'tcx>, substs: &'tcx Substs<'tcx>) -> Ty<'tcx> {
        // take a copy of substs so that we own the vectors inside
        self.mk_ty(TyStruct(def, substs))
    }

    pub fn mk_closure(&self,
                      closure_id: DefId,
                      substs: &'tcx Substs<'tcx>,
                      tys: Vec<Ty<'tcx>>)
                      -> Ty<'tcx> {
        self.mk_closure_from_closure_substs(closure_id, Box::new(ClosureSubsts {
            func_substs: substs,
            upvar_tys: tys
        }))
    }

    pub fn mk_closure_from_closure_substs(&self,
                                          closure_id: DefId,
                                          closure_substs: Box<ClosureSubsts<'tcx>>)
                                          -> Ty<'tcx> {
        self.mk_ty(TyClosure(closure_id, closure_substs))
    }

    pub fn mk_var(&self, v: TyVid) -> Ty<'tcx> {
        self.mk_infer(TyVar(v))
    }

    pub fn mk_int_var(&self, v: IntVid) -> Ty<'tcx> {
        self.mk_infer(IntVar(v))
    }

    pub fn mk_float_var(&self, v: FloatVid) -> Ty<'tcx> {
        self.mk_infer(FloatVar(v))
    }

    pub fn mk_infer(&self, it: InferTy) -> Ty<'tcx> {
        self.mk_ty(TyInfer(it))
    }

    pub fn mk_param(&self,
                    space: subst::ParamSpace,
                    index: u32,
                    name: Name) -> Ty<'tcx> {
        self.mk_ty(TyParam(ParamTy { space: space, idx: index, name: name }))
    }

    pub fn mk_self_type(&self) -> Ty<'tcx> {
        self.mk_param(subst::SelfSpace, 0, keywords::SelfType.name())
    }

    pub fn mk_param_from_def(&self, def: &ty::TypeParameterDef) -> Ty<'tcx> {
        self.mk_param(def.space, def.index, def.name)
    }

    pub fn trait_items(&self, trait_did: DefId) -> Rc<Vec<ty::ImplOrTraitItem<'tcx>>> {
        self.trait_items_cache.memoize(trait_did, || {
            let def_ids = self.trait_item_def_ids(trait_did);
            Rc::new(def_ids.iter()
                           .map(|d| self.impl_or_trait_item(d.def_id()))
                           .collect())
        })
    }

    /// Obtain the representation annotation for a struct definition.
    pub fn lookup_repr_hints(&self, did: DefId) -> Rc<Vec<attr::ReprAttr>> {
        self.repr_hint_cache.memoize(did, || {
            Rc::new(if did.is_local() {
                self.get_attrs(did).iter().flat_map(|meta| {
                    attr::find_repr_attrs(self.sess.diagnostic(), meta).into_iter()
                }).collect()
            } else {
                self.sess.cstore.repr_attrs(did)
            })
        })
    }
}
