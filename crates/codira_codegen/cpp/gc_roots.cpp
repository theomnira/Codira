//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - The two LLVM IR passes behind precise stack roots and safepoint polls
//!   (see `src/gc_roots.rs` for the design and the runtime contract):
//!   - `codira_gc_insert_roots` runs on a linked assembly module before its
//!     final optimization. In every function that makes a call that may
//!     reach a collection (a *safepoint call*), it gives the function a
//!     frame record, links it into the thread's chain on entry and unlinks
//!     it on every return, spills every value that may hold a GC reference
//!     and is live across a safepoint call into the record, and registers
//!     every stack slot that may hold one.
//!   - `codira_gc_insert_polls` runs after the final optimization and puts
//!     a poll (one load and a branch) on loop back-edges and at the entry of
//!     functions that make calls.

#include <llvm/ADT/BitVector.h>
#include <llvm/ADT/DenseMap.h>
#include <llvm/ADT/DenseSet.h>
#include <llvm/ADT/PostOrderIterator.h>
#include <llvm/ADT/SmallPtrSet.h>
#include <llvm/ADT/SmallVector.h>
#include <llvm/Analysis/AssumptionCache.h>
#include <llvm/Analysis/LoopInfo.h>
#include <llvm/Analysis/ScalarEvolution.h>
#include <llvm/Analysis/ScalarEvolutionExpressions.h>
#include <llvm/Analysis/TargetLibraryInfo.h>
#include <llvm/IR/CFG.h>
#include <llvm/IR/Constants.h>
#include <llvm/IR/DataLayout.h>
#include <llvm/IR/Dominators.h>
#include <llvm/IR/Function.h>
#include <llvm/IR/GlobalVariable.h>
#include <llvm/IR/IRBuilder.h>
#include <llvm/IR/InlineAsm.h>
#include <llvm/IR/Instructions.h>
#include <llvm/IR/IntrinsicInst.h>
#include <llvm/IR/Intrinsics.h>
#include <llvm/IR/MDBuilder.h>
#include <llvm/IR/Module.h>
#include <llvm/IR/Verifier.h>
#include <llvm/Support/ErrorHandling.h>
#include <llvm/Support/raw_ostream.h>
#include <llvm/TargetParser/Triple.h>
#include <llvm/Transforms/Utils/BasicBlockUtils.h>
#include <llvm/Transforms/Utils/ModuleUtils.h>

#include <llvm-c/Core.h>

#include <cstdint>
#include <vector>

using namespace llvm;

namespace {

// Names shared with `src/gc_roots.rs` and the runtime.
constexpr const char *CHAIN_FN_GLOBAL = "codira.gc.chain_fn";
constexpr const char *POLL_WORD_GLOBAL = "codira.gc.poll_word";
constexpr const char *POLL_FN_GLOBAL = "codira.gc.poll_fn";
constexpr const char *SET_HOOKS_FN = "set_gc_hooks";
constexpr const char *FRAME_ATTR = "codira-gc-frame";
constexpr const char *CONSERVATIVE_ATTR = "codira-gc-conservative";
constexpr const char *LEAF_ATTR = "codira-gc-leaf";
constexpr const char *FOREIGN_ATTR = "codira-gc-foreign";
constexpr const char *CONSTANT_TIME_ATTR = "codira-constant-time";

// The frame record layout (all offsets in bytes).
constexpr uint64_t REC_PREV = 0;
constexpr uint64_t REC_HEADER = 8;
constexpr uint64_t REC_LO = 16;
constexpr uint64_t REC_HI = 24;
constexpr uint64_t REC_REGIONS = 32;
constexpr uint64_t REGION_SIZE = 16;
constexpr uint64_t RECORD_MAGIC = 0xC0DAull;

bool typeHasPointer(Type *type) {
    if (type->isPointerTy()) {
        return true;
    }
    if (auto *st = dyn_cast<StructType>(type)) {
        for (Type *element : st->elements()) {
            if (typeHasPointer(element)) {
                return true;
            }
        }
        return false;
    }
    if (auto *at = dyn_cast<ArrayType>(type)) {
        return typeHasPointer(at->getElementType());
    }
    if (auto *vt = dyn_cast<VectorType>(type)) {
        return typeHasPointer(vt->getElementType());
    }
    return false;
}

/// A call that may reach a collection: anything but an LLVM intrinsic,
/// inline assembly, or a function the code generator marked as a leaf of
/// the collector.
bool isSafepointCall(const CallBase &call) {
    if (isa<IntrinsicInst>(call)) {
        return false;
    }
    if (call.isInlineAsm()) {
        return false;
    }
    if (const Function *callee = call.getCalledFunction()) {
        if (callee->isIntrinsic() || callee->hasFnAttribute(LEAF_ATTR)) {
            return false;
        }
    }
    if (call.hasFnAttr(LEAF_ATTR)) {
        return false;
    }
    return true;
}

/// Values of a function that may hold a GC reference: pointers (and
/// aggregates and vectors with pointers in them) that do not provably point
/// at a stack slot, a global or a constant, and the integers `ptrtoint`
/// makes of them.
class References {
  public:
    explicit References(Function &function) {
        SmallVector<Value *, 64> worklist;
        auto mark = [&](Value *value) {
            if (gc.insert(value).second) {
                worklist.push_back(value);
            }
        };
        for (Argument &arg : function.args()) {
            if (typeHasPointer(arg.getType())) {
                mark(&arg);
            }
        }
        for (BasicBlock &block : function) {
            for (Instruction &inst : block) {
                if (!typeHasPointer(inst.getType())) {
                    continue;
                }
                if (isa<AllocaInst>(inst) || isDerived(inst)) {
                    continue;
                }
                mark(&inst);
            }
        }
        while (!worklist.empty()) {
            Value *value = worklist.pop_back_val();
            for (User *user : value->users()) {
                auto *inst = dyn_cast<Instruction>(user);
                if (inst == nullptr || !isDerived(*inst)) {
                    continue;
                }
                if (isa<PtrToIntInst>(inst) || typeHasPointer(inst->getType())) {
                    mark(inst);
                }
            }
        }
    }

    bool contains(const Value *value) const { return gc.count(value) != 0; }

  private:
    /// Instructions whose result is a reference only if an operand is one.
    static bool isDerived(const Instruction &inst) {
        return isa<GetElementPtrInst>(inst) || isa<BitCastInst>(inst) ||
               isa<AddrSpaceCastInst>(inst) || isa<PHINode>(inst) || isa<SelectInst>(inst) ||
               isa<FreezeInst>(inst) || isa<InsertValueInst>(inst) ||
               isa<InsertElementInst>(inst) || isa<ShuffleVectorInst>(inst) ||
               isa<ExtractValueInst>(inst) || isa<ExtractElementInst>(inst) ||
               isa<PtrToIntInst>(inst);
    }

    DenseSet<const Value *> gc;
};

/// Returns true if every use of `pointer` (a stack slot, or an address
/// derived from one) leaves no GC reference in the slot: loads of
/// pointer-free values, stores of values that are not references, and
/// lifetime markers. Anything else -- a store of a reference, a load of a
/// pointer, the address escaping into a call or into memory -- may put a
/// reference there.
bool slotIsReferenceFree(Value *pointer, const References &refs, unsigned depth = 0) {
    if (depth > 16) {
        return false;
    }
    for (User *user : pointer->users()) {
        if (auto *load = dyn_cast<LoadInst>(user)) {
            if (typeHasPointer(load->getType())) {
                return false;
            }
            continue;
        }
        if (auto *store = dyn_cast<StoreInst>(user)) {
            if (store->getValueOperand() == pointer) {
                return false;  // the address escapes into memory
            }
            Value *stored = store->getValueOperand();
            if (refs.contains(stored)) {
                return false;
            }
            continue;
        }
        if (auto *intrinsic = dyn_cast<IntrinsicInst>(user)) {
            switch (intrinsic->getIntrinsicID()) {
            case Intrinsic::lifetime_start:
            case Intrinsic::lifetime_end:
            case Intrinsic::memset:
                continue;
            default:
                if (intrinsic->isAssumeLikeIntrinsic()) {
                    continue;
                }
                return false;
            }
        }
        if (isa<GetElementPtrInst>(user) || isa<BitCastInst>(user) ||
            isa<AddrSpaceCastInst>(user)) {
            if (!slotIsReferenceFree(user, refs, depth + 1)) {
                return false;
            }
            continue;
        }
        if (isa<ICmpInst>(user)) {
            continue;
        }
        return false;
    }
    return true;
}

GlobalVariable *getOrDeclarePointerGlobal(Module &module, const char *name) {
    if (GlobalVariable *global = module.getGlobalVariable(name, true)) {
        return global;
    }
    auto *ptrTy = PointerType::getUnqual(module.getContext());
    return new GlobalVariable(module, ptrTy, false, GlobalValue::ExternalLinkage, nullptr, name);
}

struct Stats {
    uint32_t precise = 0;
    uint32_t conservative = 0;
    uint32_t spilled = 0;
    uint32_t regions = 0;
};

/// Marks a function the pass cannot make precise. Its frame is scanned
/// conservatively, so neither may another function's precise frame record
/// claim its machine frame (by being inlined into it) nor may its code end
/// up inside a precise function's frame (by being inlined there).
void markConservative(Function &function) {
    // A foreign transition is only sound in a function whose references are
    // in its frame record at the call; here they may sit in callee-saved
    // registers, so the foreign call keeps the thread running.
    std::vector<CallBase *> transitions;
    for (BasicBlock &block : function) {
        for (Instruction &inst : block) {
            if (auto *call = dyn_cast<CallBase>(&inst); call && call->hasFnAttr(FOREIGN_ATTR)) {
                transitions.push_back(call);
            }
        }
    }
    for (CallBase *call : transitions) {
        call->eraseFromParent();
    }
    function.addFnAttr(CONSERVATIVE_ATTR);
    function.removeFnAttr(Attribute::AlwaysInline);
    function.addFnAttr(Attribute::NoInline);
    for (BasicBlock &block : function) {
        for (Instruction &inst : block) {
            if (auto *call = dyn_cast<CallBase>(&inst)) {
                if (!isa<IntrinsicInst>(call)) {
                    call->addFnAttr(Attribute::NoInline);
                }
            }
        }
    }
}

/// Returns true if the pass gave `function` a frame record.
bool instrumentFunction(Function &function, const DataLayout &layout, bool knowsFrameTop,
                        Stats &stats) {
    if (function.isDeclaration() || function.hasFnAttribute(FRAME_ATTR) ||
        function.hasFnAttribute(CONSERVATIVE_ATTR)) {
        return false;
    }
    if (function.hasFnAttribute(CONSTANT_TIME_ATTR)) {
        // No record (its indirect call) and no polls (their branches):
        // scanned conservatively, and kept out of precise frames.
        markConservative(function);
        stats.conservative++;
        return false;
    }

    // Which calls are safepoints, and can this function be handled at all?
    std::vector<CallBase *> safepoints;
    std::vector<CallBase *> noReturns;
    bool supported = !function.hasGC() && !function.hasPersonalityFn();
    for (BasicBlock &block : function) {
        for (Instruction &inst : block) {
            if (isa<InvokeInst>(inst) || isa<CallBrInst>(inst) || isa<IndirectBrInst>(inst)) {
                supported = false;
            }
            if (auto *call = dyn_cast<CallInst>(&inst)) {
                if (call->isMustTailCall() || call->hasFnAttr(Attribute::ReturnsTwice)) {
                    supported = false;
                }
                // Nothing is live across a call that never returns (a
                // panic): it needs no record. Should it reach a collection,
                // this frame is then scanned conservatively.
                bool returns = !call->doesNotReturn() &&
                               !isa<UnreachableInst>(call->getNextNode());
                if (isSafepointCall(*call)) {
                    (returns ? safepoints : noReturns).push_back(call);
                }
            }
        }
    }
    if (safepoints.empty()) {
        return false;
    }
    // With a record, its frame is skipped by the conservative scan, so a
    // call that does not return must still have its arguments spilled.
    safepoints.insert(safepoints.end(), noReturns.begin(), noReturns.end());

    References refs(function);

    // The record is set up after the entry block's static allocas, and it
    // names some of them: move every one of them to the top first (code
    // generation emits some in the middle of the entry block).
    BasicBlock &entry = function.getEntryBlock();
    {
        std::vector<AllocaInst *> allocas;
        for (Instruction &inst : entry) {
            if (auto *alloca = dyn_cast<AllocaInst>(&inst); alloca && alloca->isStaticAlloca()) {
                allocas.push_back(alloca);
            }
        }
        BasicBlock::iterator top = entry.getFirstInsertionPt();
        for (AllocaInst *alloca : allocas) {
            alloca->moveBefore(entry, top);
            top = std::next(alloca->getIterator());
        }
    }

    // Stack slots that may hold a reference at a safepoint.
    std::vector<AllocaInst *> regions;
    for (BasicBlock &block : function) {
        for (Instruction &inst : block) {
            auto *alloca = dyn_cast<AllocaInst>(&inst);
            if (alloca == nullptr || slotIsReferenceFree(alloca, refs)) {
                continue;
            }
            if (&block != &entry || !alloca->isStaticAlloca()) {
                supported = false;
                continue;
            }
            regions.push_back(alloca);
        }
    }
    if (!supported) {
        markConservative(function);
        stats.conservative++;
        return false;
    }

    // Number the reference values for liveness.
    std::vector<Value *> values;
    DenseMap<const Value *, unsigned> index;
    for (Argument &arg : function.args()) {
        if (refs.contains(&arg)) {
            index[&arg] = values.size();
            values.push_back(&arg);
        }
    }
    for (BasicBlock &block : function) {
        for (Instruction &inst : block) {
            if (refs.contains(&inst)) {
                index[&inst] = values.size();
                values.push_back(&inst);
            }
        }
    }
    const unsigned count = values.size();

    // Backward liveness over the reference values only. A phi's operand is
    // a use at the end of the corresponding predecessor.
    DenseMap<const BasicBlock *, BitVector> liveIn, liveOut, gen, kill;
    for (BasicBlock &block : function) {
        BitVector g(count), k(count);
        for (Instruction &inst : block) {
            if (!isa<PHINode>(inst)) {
                for (Value *operand : inst.operands()) {
                    auto found = index.find(operand);
                    if (found != index.end() && !k.test(found->second)) {
                        g.set(found->second);
                    }
                }
            }
            auto self = index.find(&inst);
            if (self != index.end()) {
                k.set(self->second);
            }
        }
        gen[&block] = g;
        kill[&block] = k;
        liveIn[&block] = BitVector(count);
        liveOut[&block] = BitVector(count);
    }
    ReversePostOrderTraversal<Function *> rpo(&function);
    std::vector<BasicBlock *> order(rpo.begin(), rpo.end());
    bool changed = true;
    while (changed) {
        changed = false;
        for (auto it = order.rbegin(); it != order.rend(); ++it) {
            BasicBlock *block = *it;
            BitVector out(count);
            for (BasicBlock *succ : successors(block)) {
                BitVector in = liveIn[succ];
                for (PHINode &phi : succ->phis()) {
                    Value *incoming = phi.getIncomingValueForBlock(block);
                    auto found = index.find(incoming);
                    if (found != index.end()) {
                        in.set(found->second);
                    }
                }
                out |= in;
            }
            BitVector in = out;
            in.reset(kill[block]);
            in |= gen[block];
            if (out != liveOut[block] || in != liveIn[block]) {
                liveOut[block] = out;
                liveIn[block] = in;
                changed = true;
            }
        }
    }

    // A value live across a safepoint call needs a slot.
    SmallPtrSet<const CallBase *, 32> isSafepoint(safepoints.begin(), safepoints.end());
    BitVector spill(count);
    for (BasicBlock &block : function) {
        BitVector live = liveOut[&block];
        for (Instruction &inst : reverse(block)) {
            if (isa<PHINode>(inst)) {
                break;
            }
            auto self = index.find(&inst);
            if (auto *call = dyn_cast<CallBase>(&inst); call && isSafepoint.count(call)) {
                BitVector across = live;
                if (self != index.end()) {
                    across.reset(self->second);
                }
                // The arguments too: one passed on the stack lives in this
                // frame's outgoing-argument area, which the collector skips,
                // and the callee may hold it only there while it runs.
                for (Value *arg : call->args()) {
                    auto found = index.find(arg);
                    if (found != index.end()) {
                        across.set(found->second);
                    }
                }
                spill |= across;
            }
            if (self != index.end()) {
                live.reset(self->second);
            }
            for (Value *operand : inst.operands()) {
                auto found = index.find(operand);
                if (found != index.end()) {
                    live.set(found->second);
                }
            }
        }
    }

    // Lay out the record: header, region descriptors, spill words.
    LLVMContext &context = function.getContext();
    Type *i8 = Type::getInt8Ty(context);
    Type *i64 = Type::getInt64Ty(context);
    auto *ptrTy = PointerType::getUnqual(context);
    uint64_t offset = REC_REGIONS + REGION_SIZE * regions.size();
    const uint64_t wordsStart = offset;
    std::vector<std::pair<Value *, uint64_t>> slots;
    for (unsigned i = 0; i < count; ++i) {
        if (!spill.test(i)) {
            continue;
        }
        Value *value = values[i];
        uint64_t size = layout.getTypeStoreSize(value->getType()).getFixedValue();
        uint64_t align = std::min<uint64_t>(16, layout.getPrefTypeAlign(value->getType()).value());
        align = std::max<uint64_t>(align, 8);
        offset = alignTo(offset, align);
        slots.emplace_back(value, offset);
        offset += alignTo(size, 8);
    }
    offset = alignTo(offset, 8);
    const uint64_t words = (offset - wordsStart) / 8;
    const uint64_t total = offset;

    // Build the record in the entry block, after its static allocas.
    BasicBlock::iterator insertAt = entry.getFirstInsertionPt();
    while (insertAt != entry.end() && isa<AllocaInst>(*insertAt)) {
        ++insertAt;
    }
    IRBuilder<> builder(&entry, insertAt);
    auto *recordTy = ArrayType::get(i8, total);
    AllocaInst *record = nullptr;
    {
        IRBuilder<> allocaBuilder(&entry, entry.getFirstInsertionPt());
        record = allocaBuilder.CreateAlloca(recordTy, nullptr, "gc.frame");
        record->setAlignment(Align(16));
    }
    auto field = [&](IRBuilder<> &b, uint64_t at) {
        return b.CreateConstInBoundsGEP1_64(i8, record, at);
    };

    GlobalVariable *chainFn = getOrDeclarePointerGlobal(*function.getParent(), CHAIN_FN_GLOBAL);
    Value *getter = builder.CreateLoad(ptrTy, chainFn, "gc.chain_fn");
    auto *getterTy = FunctionType::get(ptrTy, false);
    CallInst *head = builder.CreateCall(getterTy, getter, {}, "gc.chain");
    head->addFnAttr(Attribute::get(context, LEAF_ATTR));
    head->addFnAttr(Attribute::NoUnwind);
    Value *prev = builder.CreateLoad(ptrTy, head, "gc.prev");

    // The spill words start zeroed: the collector reads every one of them.
    if (words > 0) {
        builder.CreateMemSet(field(builder, wordsStart), builder.getInt8(0), words * 8,
                             MaybeAlign(8));
    }
    builder.CreateStore(prev, field(builder, REC_PREV));
    uint64_t header = (RECORD_MAGIC << 48) | (uint64_t(regions.size()) << 24) | words;
    builder.CreateStore(ConstantInt::get(i64, header), field(builder, REC_HEADER));
    Value *low = builder.CreateIntrinsic(Intrinsic::stacksave,
                                         {layout.getAllocaPtrType(context)}, {}, nullptr,
                                         "gc.lo");
    builder.CreateStore(low, field(builder, REC_LO));
    Value *high = ConstantPointerNull::get(ptrTy);
    if (knowsFrameTop) {
        high = builder.CreateIntrinsic(Intrinsic::addressofreturnaddress, {ptrTy}, {}, nullptr,
                                       "gc.hi");
    }
    builder.CreateStore(high, field(builder, REC_HI));
    for (size_t i = 0; i < regions.size(); ++i) {
        AllocaInst *region = regions[i];
        uint64_t size = region->getAllocationSize(layout)->getFixedValue();
        if (size >= 8) {
            builder.CreateMemSet(region, builder.getInt8(0), size, region->getAlign());
        }
        builder.CreateStore(region, field(builder, REC_REGIONS + REGION_SIZE * i));
        builder.CreateStore(ConstantInt::get(i64, size),
                            field(builder, REC_REGIONS + REGION_SIZE * i + 8));
    }
    builder.CreateStore(record, head);

    // Spill every value at its definition; the slot then holds it at every
    // safepoint it is live across (a value is never live across its own
    // definition).
    for (auto &[value, at] : slots) {
        if (auto *arg = dyn_cast<Argument>(value)) {
            (void)arg;
            builder.CreateStore(value, field(builder, at));
            continue;
        }
        auto *inst = cast<Instruction>(value);
        BasicBlock::iterator after;
        if (isa<PHINode>(inst)) {
            after = inst->getParent()->getFirstInsertionPt();
        } else {
            after = std::next(inst->getIterator());
        }
        IRBuilder<> spillBuilder(inst->getParent(), after);
        spillBuilder.CreateStore(value, field(spillBuilder, at));
    }

    // Unlink on every return.
    for (BasicBlock &block : function) {
        if (auto *ret = dyn_cast<ReturnInst>(block.getTerminator())) {
            IRBuilder<> retBuilder(ret);
            retBuilder.CreateStore(prev, head);
        }
    }

    function.addFnAttr(FRAME_ATTR);
    stats.precise++;
    stats.spilled += slots.size();
    stats.regions += regions.size();
    return true;
}

/// Defines the hook globals (pointing at private fallbacks until the
/// runtime calls `set_gc_hooks`) and `set_gc_hooks` itself.
void defineHooks(Module &module) {
    LLVMContext &context = module.getContext();
    auto *ptrTy = PointerType::getUnqual(context);

    // Fallback chain: a private head nobody reads, so a host that never
    // installs the hooks just sees every frame conservatively. Not
    // thread-local (the image writer has no thread-local sections): threads
    // then overwrite each other's links, which nothing reads.
    auto *fallbackHead = new GlobalVariable(module, ptrTy, false, GlobalValue::PrivateLinkage,
                                            ConstantPointerNull::get(ptrTy),
                                            "codira.gc.fallback_head");
    auto *getterTy = FunctionType::get(ptrTy, false);
    Function *fallbackGetter = Function::Create(getterTy, GlobalValue::PrivateLinkage,
                                                "codira.gc.fallback_chain", module);
    {
        IRBuilder<> b(BasicBlock::Create(context, "entry", fallbackGetter));
        b.CreateRet(fallbackHead);
    }
    auto *neverStop = new GlobalVariable(module, Type::getInt8Ty(context), true,
                                         GlobalValue::PrivateLinkage,
                                         ConstantInt::get(Type::getInt8Ty(context), 0),
                                         "codira.gc.never");
    auto *pollTy = FunctionType::get(Type::getVoidTy(context), false);
    Function *fallbackPoll = Function::Create(pollTy, GlobalValue::PrivateLinkage,
                                              "codira.gc.fallback_poll", module);
    {
        IRBuilder<> b(BasicBlock::Create(context, "entry", fallbackPoll));
        b.CreateRetVoid();
    }

    auto define = [&](const char *name, Constant *init) {
        GlobalVariable *global = module.getGlobalVariable(name, true);
        if (global == nullptr) {
            global = new GlobalVariable(module, ptrTy, false, GlobalValue::PrivateLinkage, init,
                                        name);
        } else {
            global->setInitializer(init);
            global->setLinkage(GlobalValue::PrivateLinkage);
        }
        return global;
    };
    GlobalVariable *chain = define(CHAIN_FN_GLOBAL, fallbackGetter);
    GlobalVariable *word = define(POLL_WORD_GLOBAL, neverStop);
    GlobalVariable *poll = define(POLL_FN_GLOBAL, fallbackPoll);
    // The poll globals have no loads until the polls are inserted, after
    // the optimizer ran: keep it from deleting them.
    appendToCompilerUsed(module, {word, poll});

    // void set_gc_hooks(void *chain_fn, void *poll_word, void *poll_fn)
    if (module.getFunction(SET_HOOKS_FN) == nullptr) {
        auto *setTy = FunctionType::get(Type::getVoidTy(context), {ptrTy, ptrTy, ptrTy}, false);
        Function *set =
            Function::Create(setTy, GlobalValue::ExternalLinkage, SET_HOOKS_FN, module);
        set->setDLLStorageClass(GlobalValue::DLLExportStorageClass);
        IRBuilder<> b(BasicBlock::Create(context, "body", set));
        b.CreateStore(set->getArg(0), chain);
        b.CreateStore(set->getArg(1), word);
        b.CreateStore(set->getArg(2), poll);
        b.CreateRetVoid();
    }
}

/// Inserts `if (*poll_word) poll_fn()`
/// before `before`.
void insertPoll(Instruction *before) {
    Module &module = *before->getModule();
    LLVMContext &context = module.getContext();
    auto *ptrTy = PointerType::getUnqual(context);
    GlobalVariable *wordGlobal = module.getGlobalVariable(POLL_WORD_GLOBAL, true);
    GlobalVariable *fnGlobal = module.getGlobalVariable(POLL_FN_GLOBAL, true);

    IRBuilder<> builder(before);
    LoadInst *wordPtr = builder.CreateLoad(ptrTy, wordGlobal, "gc.poll_word");
    wordPtr->setMetadata(LLVMContext::MD_invariant_load, MDNode::get(context, {}));
    LoadInst *word = builder.CreateAlignedLoad(builder.getInt8Ty(), wordPtr, Align(1), "gc.poll");
    word->setAtomic(AtomicOrdering::Monotonic);
    Value *stop = builder.CreateICmpNE(word, builder.getInt8(0));
    MDBuilder md(context);
    Instruction *then =
        SplitBlockAndInsertIfThen(stop, before->getIterator(), false, md.createUnlikelyBranchWeights());
    then->getParent()->setName("gc.poll.slow");
    IRBuilder<> slow(then);
    Value *fn = slow.CreateLoad(ptrTy, fnGlobal, "gc.poll_fn");
    auto *pollTy = FunctionType::get(Type::getVoidTy(context), false);
    CallInst *call = slow.CreateCall(pollTy, fn, {});
    call->addFnAttr(Attribute::Cold);
    call->addFnAttr(Attribute::NoUnwind);
    call->addFnAttr(Attribute::get(context, LEAF_ATTR));
}

bool targetKnowsFrameTop(const Module &module) {
    Triple triple(module.getTargetTriple());
    return triple.getArch() == Triple::x86_64 && !triple.isOSWindows();
}

}  // namespace

extern "C" {

/// Statistics returned by `codira_gc_insert_roots`.
struct CodiraGcRootStats {
    uint32_t precise;
    uint32_t conservative;
    uint32_t spilled;
    uint32_t regions;
};

/// Gives every function of `module_ref` that makes a safepoint call a
/// precise frame record (or marks it conservative), and defines the hooks.
void codira_gc_insert_roots(LLVMModuleRef module_ref, CodiraGcRootStats *out) {
    Module &module = *unwrap(module_ref);
    const DataLayout &layout = module.getDataLayout();
    const bool knowsFrameTop = targetKnowsFrameTop(module);
    Stats stats;
    std::vector<Function *> functions;
    for (Function &function : module) {
        functions.push_back(&function);
    }
    for (Function *function : functions) {
        if (instrumentFunction(*function, layout, knowsFrameTop, stats) &&
            verifyFunction(*function, &errs())) {
            report_fatal_error("codira_gc_insert_roots produced invalid IR in `" +
                               function->getName() + "`");
        }
    }
    defineHooks(module);
    if (out != nullptr) {
        out->precise = stats.precise;
        out->conservative = stats.conservative;
        out->spilled = stats.spilled;
        out->regions = stats.regions;
    }
}

/// Polls on the back-edges of loops whose trip count nothing bounds on
/// entry, and at the entry of every function that makes a
/// safepoint call. Returns the number of polls inserted. Requires
/// `codira_gc_insert_roots` to have defined the hooks.
uint32_t codira_gc_insert_polls(LLVMModuleRef module_ref, uint32_t max_unpolled_trips) {
    Module &module = *unwrap(module_ref);
    if (module.getGlobalVariable(POLL_WORD_GLOBAL, true) == nullptr) {
        return 0;
    }
    Triple triple(module.getTargetTriple());
    TargetLibraryInfoImpl tlii(triple);
    uint32_t polls = 0;
    for (Function &function : module) {
        if (function.isDeclaration() || function.getName().starts_with("codira.gc.") ||
            function.getName() == SET_HOOKS_FN) {
            continue;
        }
        if (function.hasFnAttribute(CONSTANT_TIME_ATTR)) {
            continue;
        }
        // Calls to constant-time functions do not count: they have no polls
        // to reach, and a poll here would put a branch next to their
        // (possibly inlined) bodies.
        bool calls = false;
        for (BasicBlock &block : function) {
            for (Instruction &inst : block) {
                if (auto *call = dyn_cast<CallBase>(&inst); call && isSafepointCall(*call)) {
                    const Function *callee = call->getCalledFunction();
                    if (callee == nullptr || !callee->hasFnAttribute(CONSTANT_TIME_ATTR)) {
                        calls = true;
                    }
                }
            }
        }

        DominatorTree dt(function);
        LoopInfo loops(dt);
        TargetLibraryInfo tli(tlii, &function);
        AssumptionCache ac(function);
        ScalarEvolution se(function, tli, ac, dt, loops);
        std::vector<Instruction *> sites;
        for (Loop *loop : loops.getLoopsInPreorder()) {
            // A loop whose trip count is bounded by a value known when it is
            // entered (a counted loop, a loop over an array, a loop that
            // may also leave early) needs no poll: it ends after at most
            // that many iterations. Only loops nothing bounds are polled.
            (void)max_unpolled_trips;
            if (!isa<SCEVCouldNotCompute>(se.getSymbolicMaxBackedgeTakenCount(loop))) {
                continue;
            }
            SmallVector<BasicBlock *, 4> latches;
            loop->getLoopLatches(latches);
            for (BasicBlock *latch : latches) {
                sites.push_back(latch->getTerminator());
            }
        }
        if (calls) {
            BasicBlock &entry = function.getEntryBlock();
            BasicBlock::iterator at = entry.getFirstInsertionPt();
            while (at != entry.end() && isa<AllocaInst>(*at)) {
                ++at;
            }
            sites.push_back(&*at);
        }
        for (Instruction *site : sites) {
            insertPoll(site);
            polls++;
        }
    }
    return polls;
}
}
