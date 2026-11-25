import Lean
import Lean.Replay

open Lean

private def exactName (text : String) : Name :=
  (text.splitOn ".").foldl Name.str .anonymous

private def moduleOf (env : Environment) (name : Name) : Option Name := do
  let idx ← env.getModuleIdxFor? name
  env.header.moduleNames[idx.toNat]?

private def requireHome (env : Environment) (mod name : Name) : IO Unit := do
  unless !isPrivateName name && moduleOf env name == some mod do
    throw <| IO.userError s!"'{name}' is not a public declaration of module '{mod}'"

private def mentions (name : Name) (type : Expr) : Bool :=
  (type.find? fun
    | .const n _ => n == name
    | _ => false).isSome

private def allDistinct (names : List Name) : Bool :=
  names.eraseDups.length == names.length

-- Replay only the audited roots and their dependency closure. The installed
-- Lean environment is imported before the built objects go on the search path.
-- Nothing in a built module may replace a trusted declaration in the replay
-- environment.
private partial def replayClosure (trusted env : Environment) (todo : List Name)
    (acc : Std.HashMap Name ConstantInfo := {}) : IO (Std.HashMap Name ConstantInfo) := do
  match todo with
  | [] => return acc
  | name :: rest =>
    if acc.contains name then
      replayClosure trusted env rest acc
    else if let some stock := trusted.find? name then
      unless moduleOf env name == moduleOf trusted name && env.find? name == some stock do
        throw <| IO.userError s!"committed declaration shadows installed Lean constant '{name}'"
      replayClosure trusted env rest acc
    else
      let some info := env.find? name
        | throw <| IO.userError s!"missing kernel declaration '{name}'"
      if info.isUnsafe || info.isPartial then
        throw <| IO.userError s!"unsafe or partial dependency '{name}'"
      if let .axiomInfo _ := info then
        throw <| IO.userError s!"non-kernel axiom '{name}'"
      let deps := info.getUsedConstantsAsSet.toList ++ (match info with
        | .inductInfo v => v.all ++ v.ctors
        | .ctorInfo v => [v.induct]
        | .recInfo v => v.all
        | _ => [])
      replayClosure trusted env (deps ++ rest) (acc.insert name info)

private def auditAxioms (env : Environment) (name : Name) : IO Unit := do
  let check : CoreM (Array Name) := Lean.collectAxioms name
  let (axioms, _) ← check.toIO { fileName := "<infra smoke audit>", fileMap := default } { env }
  for ax in axioms do
    unless ax == `propext || ax == `Classical.choice || ax == `Quot.sound do
      throw <| IO.userError s!"'{name}' depends on non-kernel axiom '{ax}'"

private def parseCount (text : String) (label : String) : IO Nat := do
  let some n := text.toNat?
    | throw <| IO.userError s!"invalid {label} count '{text}'"
  return n

def main (args : List String) : IO Unit := do
  let objects :: moduleText :: mode :: defCountText :: rest := args
    | throw <| IO.userError "missing infra audit arguments"
  unless mode == "smoke" || mode == "complete" do
    throw <| IO.userError s!"unsupported infra audit mode '{mode}'"
  let defCount ← parseCount defCountText "definition"
  unless 1 <= defCount && defCount <= 16 do
    throw <| IO.userError s!"definition count {defCount} out of range (1..=16)"
  unless rest.length >= defCount + 1 do
    throw <| IO.userError "truncated infra audit arguments"
  let defTexts := rest.take defCount
  let lemmaCountText :: lemmaTexts := rest.drop defCount
    | throw <| IO.userError "missing lemma count"
  let lemmaCount ← parseCount lemmaCountText "lemma"
  let minLemmas := if mode == "smoke" then 1 else 0
  unless minLemmas <= lemmaCount && lemmaCount <= 16 do
    throw <| IO.userError s!"lemma count {lemmaCount} out of range ({minLemmas}..=16)"
  unless lemmaTexts.length == lemmaCount do
    throw <| IO.userError "lemma argument count mismatch"

  let mod := Name.mkSimple moduleText
  let defs := defTexts.map exactName
  let lemmas := lemmaTexts.map exactName
  unless allDistinct defs do
    throw <| IO.userError "duplicate definition names"
  unless allDistinct lemmas do
    throw <| IO.userError "duplicate lemma names"

  let builtins : Array Import := #[{ module := `Lean }]
  let trusted ← Lean.importModules builtins {} (loadExts := false)

  -- Temporary object files are data only: no imported parser, elaborator,
  -- environment extension, or initializer can participate in this checker.
  let builtinPath ← Lean.searchPathRef.get
  Lean.searchPathRef.set (builtinPath ++ [System.FilePath.mk objects])
  let env ← Lean.importModules (builtins.push { module := mod }) {} (loadExts := false)

  for defName in defs do
    requireHome env mod defName
    match env.find? defName with
    | some (.defnInfo defn) =>
      unless (match defn.safety with | .safe => true | _ => false) do
        throw <| IO.userError s!"'{defName}' must be a safe definition"
    | some (.inductInfo ind) =>
      unless !ind.isUnsafe do
        throw <| IO.userError s!"'{defName}' must not be an unsafe inductive"
    | _ =>
      throw <| IO.userError s!"'{defName}' must be a public safe definition or non-unsafe inductive"

  for lemmaName in lemmas do
    requireHome env mod lemmaName
    if mode == "smoke" then
      let lemmaType ← match env.find? lemmaName with
        | some (.thmInfo thm) => pure thm.type
        | some (.defnInfo defn) =>
          unless (match defn.safety with | .safe => true | _ => false) do
            throw <| IO.userError s!"'{lemmaName}' must be a theorem or safe definition"
          pure defn.type
        | _ =>
          throw <| IO.userError s!"'{lemmaName}' must be a theorem or safe definition"
      unless defs.any (fun defName => mentions defName lemmaType) do
        throw <| IO.userError s!"statement of '{lemmaName}' does not directly reference any audited definition"
    else
      let some (.thmInfo _) := env.find? lemmaName
        | throw <| IO.userError s!"'{lemmaName}' must be a theorem"

  let roots := defs ++ lemmas
  let closure ← replayClosure trusted env roots
  let checked ← trusted.toKernelEnv.replay closure
  for name in roots do
    auditAxioms (.ofKernelEnv checked) name

  if mode == "smoke" then
    IO.println "ai4math:sealed-infra-smoke-audit:v1"
  else
    IO.println "ai4math:sealed-infra-complete-audit:v1"
