// The ICALL pass (docs/wine-native.md §3.3): native Wine code calling
// through a function pointer may be calling application (guest x86) code.
//
// Every indirect call becomes the select form measured in W0(c):
//
//   gexw_set_icall_target(p)
//   callee = p < 0x3f0000 ? p : __gexw_exit.<sig>
//   call callee(args...)
//
// A function pointer below 0x3f0000 (gex-kernel's NT trampolines) is a function-table index (native
// code), called directly; anything else is a guest address. The exit stub,
// one per wasm signature (linkonce_odr), lays the arguments out as i386
// stack words and calls __gexw_guest_call / __gexw_guest_call_fp
// (gexw_rt.c), which run the guest function through gex's function table
// and hand back EDX:EAX or ST0.
//
// Build: tools/wine-wasm/icall/build.sh. Use: clang -fpass-plugin=GexIcall.so.
// Variadic indirect calls are left alone (counted; GEXW_ICALL_STATS=1).
//
// Entry thunks (the other direction, ARM64EC's name): a native function
// whose address is taken may reach i386 code as a function-table index (a
// window procedure in a class, a COM vtable). For each such function the
// pass writes `__gexe.<name>`, an entry adapter like winebuild's (gex's
// F_body through gexrt's __gexw_entry: the arguments from the guest stack,
// the result in EAX, EDX:EAX or ST0, the arguments popped if the function
// is stdcall, which corecrt.h marks with annotate("gex.stdcall")), and
// records the pair { function, thunk } in the data section `gexw_entries`.
// gex binds each thunk at its function's table index as a guest address,
// so a guest call or jump to that index runs the native function.
#include "llvm/IR/IRBuilder.h"
#include "llvm/IR/Module.h"
#include "llvm/IR/PassManager.h"
#include "llvm/Passes/PassBuilder.h"
#include "llvm/Plugins/PassPlugin.h"
#include "llvm/Support/raw_ostream.h"
#include "llvm/Transforms/Utils/ModuleUtils.h"
#include <map>
#include <set>
#include <cstdlib>

using namespace llvm;

namespace {

constexpr uint64_t ICALL_LIMIT = 0x3f0000;

std::string sigName(FunctionType *FT, const DataLayout &DL) {
  auto t = [&](Type *T) -> std::string {
    if (T->isVoidTy()) return "v";
    if (T->isPointerTy()) return "p";
    if (T->isFloatTy()) return "f";
    if (T->isDoubleTy()) return "d";
    if (T->isIntegerTy()) return "i" + std::to_string(T->getIntegerBitWidth());
    std::string s;
    raw_string_ostream o(s);
    T->print(o);
    return "x" + std::to_string(std::hash<std::string>()(o.str()) & 0xffff);
  };
  std::string s = "__gexw_exit." + t(FT->getReturnType()) + "_";
  for (Type *P : FT->params()) s += t(P);
  return s;
}

struct GexIcall : PassInfoMixin<GexIcall> {
  Function *getTarget(Module &M, const char *name, FunctionType *FT) {
    return cast<Function>(M.getOrInsertFunction(name, FT).getCallee());
  }

  // The exit stub for calls of type FT (byval attributes are taken from the
  // first call site seen; they are part of the C type in practice).
  Function *exitStub(Module &M, CallBase &CB) {
    FunctionType *FT = CB.getFunctionType();
    const DataLayout &DL = M.getDataLayout();
    std::string name = sigName(FT, DL);
    // byval parameters change the stack layout: give them their own stubs.
    for (unsigned i = 0; i < FT->getNumParams(); i++)
      if (Type *BT = CB.getParamByValType(i))
        name += ".bv" + std::to_string(i) + "_" + std::to_string(DL.getTypeAllocSize(BT));
    if (Function *F = M.getFunction(name)) return F;

    LLVMContext &C = M.getContext();
    Type *I32 = Type::getInt32Ty(C), *I64 = Type::getInt64Ty(C);
    Type *Ptr = PointerType::get(C, 0);
    Function *F = Function::Create(FT, GlobalValue::LinkOnceODRLinkage, name, M);
    F->setVisibility(GlobalValue::HiddenVisibility);
    F->addFnAttr(Attribute::NoInline);
    F->addFnAttr("gexw-no-icall");
    BasicBlock *BB = BasicBlock::Create(C, "entry", F);
    IRBuilder<> B(BB);

    // Count the i386 stack words.
    unsigned words = 0;
    for (unsigned i = 0; i < FT->getNumParams(); i++) {
      Type *P = FT->getParamType(i);
      if (Type *BT = CB.getParamByValType(i)) words += (DL.getTypeAllocSize(BT) + 3) / 4;
      else if (P->isIntegerTy(64) || P->isDoubleTy()) words += 2;
      else words += 1;
    }
    Value *Buf = B.CreateAlloca(I32, B.getInt32(words ? words : 1));
    unsigned w = 0;
    for (unsigned i = 0; i < FT->getNumParams(); i++) {
      Argument *A = F->getArg(i);
      Type *P = A->getType();
      auto slot = [&](unsigned k) { return B.CreateGEP(I32, Buf, B.getInt32(k)); };
      if (Type *BT = CB.getParamByValType(i)) {
        uint64_t sz = DL.getTypeAllocSize(BT);
        B.CreateMemCpy(slot(w), Align(4), A, Align(1), sz);
        w += (sz + 3) / 4;
      } else if (P->isIntegerTy(64) || P->isDoubleTy()) {
        Value *V = P->isDoubleTy() ? B.CreateBitCast(A, I64) : (Value *)A;
        B.CreateStore(B.CreateTrunc(V, I32), slot(w));
        B.CreateStore(B.CreateTrunc(B.CreateLShr(V, 32), I32), slot(w + 1));
        w += 2;
      } else if (P->isPointerTy()) {
        B.CreateStore(B.CreatePtrToInt(A, I32), slot(w++));
      } else if (P->isFloatTy()) {
        B.CreateStore(B.CreateBitCast(A, I32), slot(w++));
      } else if (P->isIntegerTy()) {
        // i386 passes narrow integers widened to a word.
        B.CreateStore(B.CreateZExtOrTrunc(A, I32), slot(w++));
      } else {
        report_fatal_error("gex icall: unsupported parameter type in " + Twine(name));
      }
    }
    Function *Get = getTarget(M, "gexw_get_icall_target", FunctionType::get(I32, false));
    Value *T = B.CreateCall(Get);
    Type *R = FT->getReturnType();
    if (R->isFloatingPointTy()) {
      Function *Call = getTarget(M, "__gexw_guest_call_fp", FunctionType::get(B.getDoubleTy(), {I32, Ptr, I32}, false));
      Value *V = B.CreateCall(Call, {T, Buf, B.getInt32(words)});
      B.CreateRet(R->isFloatTy() ? B.CreateFPTrunc(V, R) : V);
    } else {
      Function *Call = getTarget(M, "__gexw_guest_call", FunctionType::get(I64, {I32, Ptr, I32}, false));
      Value *V = B.CreateCall(Call, {T, Buf, B.getInt32(words)});
      if (R->isVoidTy()) B.CreateRetVoid();
      else if (R->isPointerTy()) B.CreateRet(B.CreateIntToPtr(B.CreateTrunc(V, I32), R));
      else if (R->isIntegerTy()) B.CreateRet(B.CreateTrunc(V, R));
      else report_fatal_error("gex icall: unsupported return type in " + Twine(name));
    }
    return F;
  }

  // Is F's address taken by anything other than calls and metadata?
  static bool escapes(Function &F) {
    for (const Use &U : F.uses()) {
      const User *Us = U.getUser();
      if (auto *CB = dyn_cast<CallBase>(Us))
        if (CB->isCallee(&U)) continue;
      if (auto *GV = dyn_cast<GlobalVariable>(Us))
        if (GV->getSection() == "llvm.metadata") continue;
      if (auto *CE = dyn_cast<Constant>(Us)) {
        // annotation and used-list entries are constant structs/arrays
        // reachable only from llvm.metadata globals
        bool meta = true;
        SmallVector<const User *, 8> work{CE};
        while (!work.empty() && meta) {
          const User *X = work.pop_back_val();
          if (auto *G = dyn_cast<GlobalVariable>(X)) {
            if (G->getSection() != "llvm.metadata" && G->getName() != "llvm.used" &&
                G->getName() != "llvm.compiler.used")
              meta = false;
            continue;
          }
          if (!isa<Constant>(X)) { meta = false; break; }
          if (X->use_empty()) continue;
          for (const User *Y : X->users()) work.push_back(Y);
        }
        if (meta) continue;
      }
      return true;
    }
    return false;
  }

  // The i386 convention a native function has towards the guest, from its
  // annotation (llvm.global.annotations): corecrt.h/minwindef.h's
  // __stdcall, __thiscall, __fastcall on wasm32.
  enum Conv { CDECL, STDCALL, THISCALL, FASTCALL };
  static std::map<Function *, Conv> conventions(Module &M) {
    std::map<Function *, Conv> out;
    GlobalVariable *A = M.getGlobalVariable("llvm.global.annotations");
    if (!A || !A->hasInitializer()) return out;
    auto *Arr = dyn_cast<ConstantArray>(A->getInitializer());
    if (!Arr) return out;
    for (Value *Op : Arr->operands()) {
      auto *S = dyn_cast<ConstantStruct>(Op);
      if (!S || S->getNumOperands() < 2) continue;
      auto *F = dyn_cast<Function>(S->getOperand(0)->stripPointerCasts());
      auto *Str = dyn_cast<GlobalVariable>(S->getOperand(1)->stripPointerCasts());
      if (!F || !Str || !Str->hasInitializer()) continue;
      if (auto *CDA = dyn_cast<ConstantDataArray>(Str->getInitializer())) {
        if (!CDA->isCString()) continue;
        StringRef a = CDA->getAsCString();
        if (a == "gex.stdcall") out.emplace(F, STDCALL);
        else if (a == "gex.thiscall") out[F] = THISCALL;
        else if (a == "gex.fastcall") out[F] = FASTCALL;
      }
    }
    return out;
  }

  // The entry thunk of F (see the top), or null if its signature has no
  // i386 shape here.
  Function *entryThunk(Module &M, Function &F, Conv conv) {
    LLVMContext &C = M.getContext();
    const DataLayout &DL = M.getDataLayout();
    Type *I32 = Type::getInt32Ty(C), *I64 = Type::getInt64Ty(C), *F32 = Type::getFloatTy(C),
         *F64 = Type::getDoubleTy(C);
    Type *Ptr = PointerType::get(C, 0);
    FunctionType *FT = F.getFunctionType();
    if (FT->isVarArg()) return nullptr;
    FunctionType *BodyT = FunctionType::get(I32, {I32, I32, I32, I64}, false);
    std::string base = "__gexe." + F.getName().str();
    Function *Body = Function::Create(BodyT, GlobalValue::InternalLinkage, base + ".body", M);
    Body->addFnAttr("gexw-no-icall");
    BasicBlock *BB = BasicBlock::Create(C, "entry", Body);
    IRBuilder<> B(BB);
    Value *Ctx = Body->getArg(0), *Ret = Body->getArg(3);
    auto helper = [&](const char *n, Type *R, ArrayRef<Type *> P) {
      return M.getOrInsertFunction(n, FunctionType::get(R, P, false));
    };
    unsigned off = 0, first = 0;
    std::vector<Value *> args;
    // A struct return: 1, 2, 4 or 8 bytes come back in EDX:EAX (MSVC's
    // i386 rule); larger ones through the caller's hidden pointer, the
    // first stack word, which is also the result.
    Value *SRet = nullptr;
    uint64_t sretSize = 0;
    bool sretReg = false;
    if (FT->getNumParams() && F.hasParamAttribute(0, Attribute::StructRet)) {
      Type *ST = F.getParamStructRetType(0);
      sretSize = DL.getTypeAllocSize(ST);
      sretReg = sretSize == 1 || sretSize == 2 || sretSize == 4 || sretSize == 8;
      if (sretReg) {
        SRet = B.CreateAlloca(I64);
        B.CreateStore(B.getInt64(0), SRet);
        args.push_back(SRet);
      } else {
        SRet = B.CreateCall(helper("__gexw_arg32", I32, {I32, I32}), {Ctx, B.getInt32(0)});
        args.push_back(B.CreateIntToPtr(SRet, Ptr));
        off = 4;
      }
      first = 1;
    }
    // thiscall: `this` in ECX; fastcall: the first two word-sized integer
    // or pointer arguments in ECX and EDX. The rest are on the stack, and
    // the callee pops them (as stdcall).
    unsigned regs = 0, maxregs = conv == THISCALL ? 1 : conv == FASTCALL ? 2 : 0;
    for (unsigned i = first; i < FT->getNumParams(); i++) {
      Type *P = FT->getParamType(i);
      if (regs < maxregs && !F.getParamByValType(i) &&
          (P->isPointerTy() || (P->isIntegerTy() && P->getIntegerBitWidth() <= 32))) {
        Value *A = B.CreateCall(helper("__gexw_reg", I32, {I32, I32}), {Ctx, B.getInt32(regs == 0 ? 1 : 2)});
        args.push_back(P->isPointerTy() ? B.CreateIntToPtr(A, P) : B.CreateTrunc(A, P));
        regs++;
        continue;
      }
      regs = maxregs;  /* fastcall: past a stack argument, the rest are too */
      if (Type *BT = F.getParamByValType(i)) {
        Value *A = B.CreateCall(helper("__gexw_argptr", I32, {I32, I32}), {Ctx, B.getInt32(off)});
        args.push_back(B.CreateIntToPtr(A, Ptr));
        off += (DL.getTypeAllocSize(BT) + 3) & ~3u;
      } else if (P->isIntegerTy(64)) {
        args.push_back(B.CreateCall(helper("__gexw_arg64", I64, {I32, I32}), {Ctx, B.getInt32(off)}));
        off += 8;
      } else if (P->isDoubleTy()) {
        args.push_back(B.CreateCall(helper("__gexw_argf64", F64, {I32, I32}), {Ctx, B.getInt32(off)}));
        off += 8;
      } else if (P->isFloatTy()) {
        args.push_back(B.CreateCall(helper("__gexw_argf32", F32, {I32, I32}), {Ctx, B.getInt32(off)}));
        off += 4;
      } else if (P->isPointerTy()) {
        Value *A = B.CreateCall(helper("__gexw_arg32", I32, {I32, I32}), {Ctx, B.getInt32(off)});
        args.push_back(B.CreateIntToPtr(A, P));
        off += 4;
      } else if (P->isIntegerTy() && P->getIntegerBitWidth() <= 32) {
        Value *A = B.CreateCall(helper("__gexw_arg32", I32, {I32, I32}), {Ctx, B.getInt32(off)});
        args.push_back(B.CreateTrunc(A, P));
        off += 4;
      } else {
        Body->eraseFromParent();
        return nullptr;
      }
    }
    unsigned pop = conv == CDECL ? 0 : off;
    Value *R = B.CreateCall(FT, &F, args);
    Type *RT = FT->getReturnType();
    Value *Out;
    if (SRet && sretReg) {
      Value *V = B.CreateLoad(I64, SRet);
      Out = B.CreateCall(helper("__gexw_ret", I32, {I32, I64, I32, I64}), {Ctx, Ret, B.getInt32(pop), V});
    } else if (SRet) {
      Out = B.CreateCall(helper("__gexw_ret32", I32, {I32, I64, I32, I32}), {Ctx, Ret, B.getInt32(pop), SRet});
    } else if (RT->isVoidTy()) {
      Out = B.CreateCall(helper("__gexw_ret32", I32, {I32, I64, I32, I32}), {Ctx, Ret, B.getInt32(pop), B.getInt32(0)});
    } else if (RT->isFloatTy() || RT->isDoubleTy()) {
      Value *V = RT->isFloatTy() ? B.CreateFPExt(R, F64) : R;
      Out = B.CreateCall(helper("__gexw_ret_fp", I32, {I32, I64, I32, F64}), {Ctx, Ret, B.getInt32(pop), V});
    } else if (RT->isIntegerTy(64)) {
      Out = B.CreateCall(helper("__gexw_ret", I32, {I32, I64, I32, I64}), {Ctx, Ret, B.getInt32(pop), R});
    } else if (RT->isPointerTy() || (RT->isIntegerTy() && RT->getIntegerBitWidth() <= 32)) {
      Value *V = RT->isPointerTy() ? B.CreatePtrToInt(R, I32) : B.CreateZExtOrTrunc(R, I32);
      Out = B.CreateCall(helper("__gexw_ret32", I32, {I32, I64, I32, I32}), {Ctx, Ret, B.getInt32(pop), V});
    } else {
      Body->eraseFromParent();
      return nullptr;
    }
    B.CreateRet(Out);

    Function *T = Function::Create(BodyT, GlobalValue::InternalLinkage, base, M);
    T->addFnAttr("gexw-no-icall");
    IRBuilder<> TB(BasicBlock::Create(C, "entry", T));
    FunctionCallee Entry = helper("__gexw_entry", I32, {I32, I32, I32, I64, Ptr});
    TB.CreateRet(TB.CreateCall(Entry, {T->getArg(0), T->getArg(1), T->getArg(2), T->getArg(3), Body}));
    return T;
  }

  // The pairs as a used, internal array in `section`.
  static void emitPairs(Module &M, const std::vector<std::pair<Function *, Function *>> &pairs,
                        const char *name, const char *section) {
    if (pairs.empty()) return;
    LLVMContext &C = M.getContext();
    Type *Ptr = PointerType::get(C, 0);
    StructType *PairT = StructType::get(Ptr, Ptr);
    std::vector<Constant *> elems;
    for (auto &[F, T] : pairs) elems.push_back(ConstantStruct::get(PairT, {F, T}));
    ArrayType *AT = ArrayType::get(PairT, elems.size());
    auto *G = new GlobalVariable(M, AT, true, GlobalValue::InternalLinkage, ConstantArray::get(AT, elems), name);
    G->setSection(section);
    G->setAlignment(Align(4));
    appendToUsed(M, {G});
  }

  unsigned entryThunks(Module &M) {
    std::map<Function *, Conv> cv = conventions(M);
    std::vector<std::pair<Function *, Function *>> pairs, decl_pairs;
    std::vector<Function *> fs, decls;
    for (Function &F : M) {
      if (F.hasFnAttribute("gexw-no-icall") || F.isIntrinsic()) continue;
      StringRef n = F.getName();
      if (n.starts_with("__gexw") || n.starts_with("__gexe.") || n.contains("gexp_") || n.starts_with("llvm.")) continue;
      if (F.isDeclaration()) {
        // A function defined elsewhere whose address this object hands out
        // (a table of server routines next to their definitions in another
        // object, a function imported from another DLL): a thunk of its
        // own, which gex binds only when no object defining the function
        // gave one (`gexw_entries_decl`): the definition knows its
        // convention, this object may not.
        if (escapes(F)) decls.push_back(&F);
        continue;
      }
      // (a thiscall method may be referenced only from a vtable written in
      // assembly, which the IR does not show: msvcrt's and msvcp's)
      bool method = cv.count(&F) && cv[&F] == THISCALL && !F.hasLocalLinkage();
      if (escapes(F) || method) fs.push_back(&F);
    }
    for (Function *F : fs)
      if (Function *T = entryThunk(M, *F, cv.count(F) ? cv[F] : CDECL))
        pairs.push_back({F, T});
    for (Function *F : decls)
      if (Function *T = entryThunk(M, *F, cv.count(F) ? cv[F] : CDECL))
        decl_pairs.push_back({F, T});
    emitPairs(M, pairs, "__gexe.pairs", "gexw_entries");
    emitPairs(M, decl_pairs, "__gexe.declpairs", "gexw_entries_decl");
    return pairs.size() + decl_pairs.size();
  }

  PreservedAnalyses run(Module &M, ModuleAnalysisManager &) {
    LLVMContext &C = M.getContext();
    Type *I32 = Type::getInt32Ty(C);
    unsigned thunks = getenv("GEXW_NO_ENTRY_THUNKS") ? 0 : entryThunks(M);
    FunctionType *SetT = FunctionType::get(Type::getVoidTy(C), {I32}, false);
    unsigned done = 0, varargs = 0;
    std::vector<CallBase *> sites;
    for (Function &F : M) {
      if (F.isDeclaration() || F.hasFnAttribute("gexw-no-icall")) continue;
      for (BasicBlock &BB : F)
        for (Instruction &I : BB)
          if (auto *CB = dyn_cast<CallBase>(&I))
            if (!CB->getCalledFunction() && !CB->isInlineAsm() &&
                !isa<Function>(CB->getCalledOperand()->stripPointerCasts()))
              sites.push_back(CB);
    }
    for (CallBase *CB : sites) {
      if (CB->getFunctionType()->isVarArg()) {
        varargs++;
        continue;
      }
      Function *Stub = exitStub(M, *CB);
      IRBuilder<> B(CB);
      Value *Callee = CB->getCalledOperand();
      Value *P = B.CreatePtrToInt(Callee, I32);
      B.CreateCall(M.getOrInsertFunction("gexw_set_icall_target", SetT), {P});
      Value *Native = B.CreateICmpULT(P, B.getInt32(ICALL_LIMIT));
      CB->setCalledOperand(B.CreateSelect(Native, Callee, Stub));
      done++;
    }
    if (getenv("GEXW_ICALL_STATS"))
      errs() << "gex icall: " << M.getName() << ": " << done << " sites, " << varargs << " variadic left alone, "
             << thunks << " entry thunks\n";
    return done || thunks ? PreservedAnalyses::none() : PreservedAnalyses::all();
  }
};

} // namespace

extern "C" LLVM_ATTRIBUTE_WEAK PassPluginLibraryInfo llvmGetPassPluginInfo() {
  return {LLVM_PLUGIN_API_VERSION, "GexIcall", "0.1", [](PassBuilder &PB) {
            PB.registerOptimizerLastEPCallback(
                [](ModulePassManager &MPM, OptimizationLevel, ThinOrFullLTOPhase) { MPM.addPass(GexIcall()); });
          }};
}
