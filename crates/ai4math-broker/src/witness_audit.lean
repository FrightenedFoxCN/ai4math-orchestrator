open Lean Elab Command in
elab "#ai4math_check_sealed_witness" : command => do
  let env ← getEnv
  let some (.thmInfo witness) := env.find? `witness
    | throwError "missing kernel theorem witness"
  if env.isImportedConst `witness then
    throwError "witness theorem must come from this check"
  let .ok expected := ai4mathSealedWitnessType env
    | throwError "missing sealed witness type"
  unless witness.levelParams.isEmpty && witness.type == expected do
    throwError "kernel witness theorem does not have the sealed refutation type"
  -- Audit this exact kernel declaration, including dependencies of its type.
  let axioms ← Lean.collectAxioms `witness
  if axioms.isEmpty then
    logInfo "'witness' does not depend on any axioms"
  else
    let names := String.intercalate ", " (axioms.map Name.toString).toList
    logInfo m!"'witness' depends on axioms: [{names}]"
