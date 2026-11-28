import Lean
import Lean.Replay

open Lean

private def exactName (text : String) : Name :=
  (text.splitOn ".").foldl Name.str .anonymous

private def moduleOf (env : Environment) (name : Name) : Option Name := do
  let idx ← env.getModuleIdxFor? name
  env.header.moduleNames[idx.toNat]?

-- Check the public artifact and the full kernel declaration from the exact owning
-- module. Imported environments can merge entries for duplicate theorem names.
private def ownPublicDecl (objects : System.FilePath) (env : Environment)
    (mod name : Name) : IO ConstantInfo := do
  let badOrigin := IO.userError s!"'{name}' is not a public declaration of module '{mod}'"
  let some idx := env.getModuleIdxFor? name | throw badOrigin
  unless !isPrivateName name && env.header.moduleNames[idx.toNat]? == some mod do
    throw badOrigin
  let some data := env.header.moduleData[idx.toNat]? | throw badOrigin
  let some info := data.constants.find? (·.name == name) | throw badOrigin
  unless env.find? name == some info do
    throw <| IO.userError s!"ambiguous kernel declaration '{name}'"
  let (exported, _) ← Lean.readModuleData (objects / (mod.toString ++ ".olean"))
  let some publicInfo := exported.constants.find? (·.name == name) | throw badOrigin
  unless publicInfo.toConstantVal == info.toConstantVal do
    throw badOrigin
  return info

-- Only stock declarations are trusted. Never inherit the candidate's kernel
-- flags, axioms, or elaborator extensions when checking its dependency closure.
private partial def replayClosure (trusted env : Environment) (todo : List Name) (allowOracle : Bool)
    (acc : Std.HashMap Name ConstantInfo := {}) : IO (Std.HashMap Name ConstantInfo) := do
  match todo with
  | [] => return acc
  | name :: rest =>
    if acc.contains name then
      replayClosure trusted env rest allowOracle acc
    else if let some stock := trusted.find? name then
      unless moduleOf env name == moduleOf trusted name && env.find? name == some stock do
        throw <| IO.userError s!"source shadows installed Lean constant '{name}'"
      replayClosure trusted env rest allowOracle acc
    else
      let some info := env.find? name
        | throw <| IO.userError s!"missing kernel declaration '{name}'"
      if info.isUnsafe || info.isPartial then
        throw <| IO.userError s!"unsafe or partial dependency '{name}'"
      if let .axiomInfo _ := info then
        unless allowOracle && name.toString.contains "._native.native_decide.ax_" do
          throw <| IO.userError s!"non-kernel axiom '{name}'"
      let deps := info.getUsedConstantsAsSet.toList ++ (match info with
        | .inductInfo v => v.all
        | .ctorInfo v => [v.induct]
        | _ => [])
      replayClosure trusted env (deps ++ rest) allowOracle (acc.insert name info)

private def auditAxioms (env : Environment) (name : Name) (allowSorry allowOracle : Bool) : IO Unit := do
  let check : CoreM (Array Name) := Lean.collectAxioms name
  let (axioms, _) ← check.toIO { fileName := "<reviewed spec comparison>", fileMap := default } { env }
  for ax in axioms do
    unless ax == `propext || ax == `Classical.choice || ax == `Quot.sound
      || (allowSorry && ax == `sorryAx)
      || (allowOracle && (ax == `Lean.ofReduceBool || ax == `Lean.trustCompiler
        || ax.toString.startsWith (name.toString ++ "._native.native_decide.ax_"))) do
      throw <| IO.userError s!"'{name}' depends on non-kernel axiom '{ax}'"

def main (args : List String) : IO Unit := do
  let mode :: args := args | throw <| IO.userError "missing spec audit mode"
  let allowSorry := mode == "sketch"
  let allowOracle := mode == "oracle"
  unless mode == "strict" || allowSorry || allowOracle do
    throw <| IO.userError "invalid spec audit mode"
  let [prefixPath, fullPath, specModule, specText, candidateModule, candidateText] := args
    | throw <| IO.userError "invalid spec audit arguments"
  let smod := Name.mkSimple specModule
  let cmod := Name.mkSimple candidateModule
  let spec := exactName specText
  let candidate := exactName candidateText
  unless smod != cmod do
    throw <| IO.userError "spec module must be independent from candidate"
  let builtins : Array Import := #[{ module := `Lean }]
  let trusted ← Lean.importModules builtins {} (loadExts := false)
  let builtinPath ← Lean.searchPathRef.get
  let imports := builtins.push { module := smod }

  -- Resolve the reviewed spec using only artifacts through its own module.
  -- In particular, a later source cannot supply any missing kernel constants.
  Lean.searchPathRef.set (builtinPath ++ [System.FilePath.mk prefixPath])
  let prefixEnv ← Lean.importModules imports {} (loadExts := false)
  let .defnInfo defn ← ownPublicDecl (.mk prefixPath) prefixEnv smod spec
    | throw <| IO.userError "reviewed declaration must be a public definition"
  unless defn.safety == .safe && defn.levelParams.isEmpty && defn.type == .sort .zero do
    throw <| IO.userError "reviewed declaration must be a safe monomorphic definition of type Prop"
  let specClosure ← replayClosure trusted prefixEnv [spec] false

  Lean.searchPathRef.set (builtinPath ++ [System.FilePath.mk fullPath])
  let full ← Lean.importModules (imports.push { module := cmod }) {} (loadExts := false)
  let .thmInfo thm ← ownPublicDecl (.mk fullPath) full cmod candidate
    | throw <| IO.userError "candidate declaration must be a public theorem"
  for (name, info) in specClosure.toList do
    unless full.find? name == some info && moduleOf full name == moduleOf prefixEnv name do
      throw <| IO.userError s!"candidate environment changes the reviewed spec dependency '{name}'"
  let closure ← replayClosure trusted full [candidate] allowOracle specClosure
  let checked ← trusted.toKernelEnv.replay closure
  auditAxioms (.ofKernelEnv checked) spec false false
  auditAxioms (.ofKernelEnv checked) candidate allowSorry allowOracle

  -- The value is the bare theorem constant (with its original universe params),
  -- not an elaborated term. The trusted native kernel accepts this declaration
  -- only if the inferred theorem type is definitionally equal to defn.value.
  -- There are no term arguments, coercions, tactics, or proofs of implication.
  let auditDecl := Declaration.thmDecl {
    name := Name.num (Name.mkSimple "ai4mathSealedLeanSpecComparison") 0
    levelParams := thm.levelParams
    type := defn.value
    value := .const candidate (thm.levelParams.map Level.param)
  }
  match checked.addDeclCore 0 0 auditDecl (cancelTk? := none) with
  | .error _ =>
    throw <| IO.userError "candidate theorem type is not definitionally equal to the reviewed spec value"
  | .ok _ => IO.println "ai4math:sealed-lean-spec-comparison:v1"
