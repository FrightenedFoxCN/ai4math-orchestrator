// Client side of the Typert descriptor catalog. The Host separately validates
// requests and permissions; this layer exposes only the typed Remote namespace.
function checked(schema, value) {
  const invalid = () => { throw new TypeError('Invalid AI4Math Remote payload') }
  if (schema.enum && !schema.enum.includes(value)) invalid()
  switch (schema.type) {
    case 'object': {
      if (!value || typeof value !== 'object' || Array.isArray(value)) invalid()
      const fields = schema.properties ?? {}
      for (const key of schema.required ?? []) if (!Object.hasOwn(value, key)) invalid()
      for (const [key, item] of Object.entries(value)) {
        if (Object.hasOwn(fields, key)) checked(fields[key], item)
        else if (schema.additionalProperties === false) invalid()
      }
      break
    }
    case 'array':
      if (!Array.isArray(value) || value.length < (schema.minItems ?? 0)
        || value.length > (schema.maxItems ?? Infinity)
        || (schema.uniqueItems && new Set(value).size !== value.length)) invalid()
      if (schema.items) for (const item of value) checked(schema.items, item)
      break
    case 'integer':
      if (!Number.isSafeInteger(value) || value < (schema.minimum ?? -Infinity)
        || value > (schema.maximum ?? Infinity)) invalid()
      break
    case 'boolean':
      if (typeof value !== 'boolean') invalid()
      break
    case 'string': {
      if (typeof value !== 'string') invalid()
      const size = Array.from(value).length
      if (size < (schema.minLength ?? 0) || size > (schema.maxLength ?? Infinity)) invalid()
      const wikiMax = schema['x-ai4math-maxUtf8Bytes']
      if (wikiMax !== undefined && (new TextEncoder().encode(value).byteLength > wikiMax || !new RegExp(schema.pattern, 'u').test(value))) invalid()
      break
    }
  }
  return value
}

function strictCodec(typeSymbol, jsonSchema) {
  const parse = value => checked(jsonSchema, value)
  return {
    mode: 'strict', typeSymbol, jsonSchema,
    create: () => ({ parse, safeParse(value) {
      try { return { success: true, data: parse(value) } }
      catch (error) { return { success: false, error } }
    } }),
  }
}

function contribution({ REMOTE_PACKAGE, REMOTE_NAMESPACE, REMOTE_METHODS, REMOTE_SERVICE = REMOTE_NAMESPACE }) {
  return {
    package: REMOTE_PACKAGE,
    descriptors: Object.entries(REMOTE_METHODS).filter(([, spec]) => !spec.agentOnly).map(([method, spec]) => {
      const id = REMOTE_NAMESPACE + '/' + method
      return {
        id: REMOTE_PACKAGE + '#' + id, service: REMOTE_SERVICE, namespace: REMOTE_NAMESPACE, method,
        ...(spec.stream ? { mode: 'stream' } : {}),
        invocation: { kind: 'direct' }, description: spec.description,
        parameters: [{ name: 'params', wire: 'params', source: 'json',
          codec: strictCodec(id + ':params', spec.params) }],
        cancellation: { parameter: 'signal' },
        // The Host checks the full wire shape. All consumers also guard fields
        // before display; expose no raw markup and render no trusted links.
        result: strictCodec(id + ':result', { type: 'object' }),
      }
    }),
  }
}

async function call(remote, method, params, signal) {
  const reply = await remote[method](params, signal)
  signal.throwIfAborted()
  if (reply?.ok !== true || !reply.value || typeof reply.value !== 'object' || Array.isArray(reply.value)) {
    throw new Error('AI4Math data unavailable')
  }
  return reply.value
}

module.exports = { call, contribution }
