#!/usr/bin/env python3
"""Render this repository's finite OpenAPI model profile into shared Rust DTOs.

This is deliberately not a general OpenAPI client generator. It emits serde wire
shapes and typed requests; admission owns numeric/semantic authorization.
Unsupported type constructs fail rather than silently generating Value.
"""
import argparse
import json
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[1]
SPEC = ROOT / 'api/openapi.json'
CLIENT = ROOT / "crates/sandbox-client/src"
OUTPUT = ROOT / 'crates/sandbox-protocol/src/api.rs'


def nullable(schema):
    if 'anyOf' not in schema:
        return None
    parts = schema['anyOf']
    if len(parts) != 2 or parts[1] != {'type': 'null'}:
        raise ValueError('Only explicit T/null unions are supported by the Rust model profile')
    return parts[0]


def rust_type(schema, schemas):
    if any(key in schema for key in ("oneOf", "allOf", "if", "then", "else")):
        raise ValueError("Unsupported type composition in Rust model profile")
    if schema.get('x-rust-json'):
        return 'serde_json::Value'
    inner = nullable(schema)
    if inner is not None:
        return f'Option<{rust_type(inner, schemas)}>'
    if '$ref' in schema:
        name = schema['$ref'].removeprefix('#/components/schemas/')
        target = schemas[name]
        return name if target.get('x-rust-model') else rust_type(target, schemas)
    kind = schema.get('type')
    if kind == 'string':
        return 'String'
    if kind == 'boolean':
        return 'bool'
    if kind == 'integer':
        name = schema.get('x-rust-integer', 'i64')
        if name not in ('i32', 'i64', 'u16', 'u32', 'u64'):
            raise ValueError(f'Unsupported integer {name}')
        return name
    if kind == 'array':
        return f'Vec<{rust_type(schema["items"], schemas)}>'
    if kind == 'object' and 'properties' not in schema:
        additional = schema.get('additionalProperties')
        if isinstance(additional, dict):
            return f'std::collections::BTreeMap<String, {rust_type(additional, schemas)}>'
        if additional is True:
            return 'serde_json::Value'
    raise ValueError(f'Unsupported Rust schema shape: {schema}')


def literal(value, ty):
    if ty == 'String' and isinstance(value, str):
        # json string escapes differ from Rust for controls: defaults are printable only.
        if any(ord(c) < 32 or ord(c) > 126 for c in value):
            raise ValueError('Non-ASCII default needs explicit generator support')
        return json.dumps(value) + '.into()'
    if type(value) is bool and ty == 'bool':
        return str(value).lower()
    if type(value) is int and ty in ('i32', 'i64', 'u16', 'u32', 'u64'):
        return str(value)
    if value == {} and ty.startswith('std::collections::BTreeMap<'):
        return 'Default::default()'
    raise ValueError(f'Unsupported default {value!r} for {ty}')


def render(doc):
    lines = ['//! Generated from api/openapi.json by scripts/generate_api.py. Do not edit.',
             '//! Wire shapes only; bounds, ownership and lifecycle checks remain in admission.',
             'use serde::{Deserialize, Serialize};',
             "fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>",
             "where D: serde::Deserializer<'de>, T: Deserialize<'de> { Option::deserialize(deserializer) }", '']
    schemas = doc['components']['schemas']
    for name, schema in schemas.items():
        if not schema.get('x-rust-model'):
            continue
        if not re.fullmatch(r'[A-Z][A-Za-z0-9]*', name):
            raise ValueError(f'Invalid model name: {name}')
        if any(key in schema for key in ('oneOf', 'allOf', 'anyOf', 'if', 'then', 'else')):
            raise ValueError('Unsupported root model composition')
        if schema.get('type') != 'object':
            raise ValueError('Generated roots must be named objects')
        derives = ['Clone', 'Serialize', 'Deserialize']
        if not schema.get('x-rust-redacted-debug'):
            derives.insert(0, 'Debug')
        if schema.get('x-rust-copy'):
            derives.append('Copy')
        lines.append('#[derive(' + ', '.join(derives) + ')]')
        if schema.get('additionalProperties') is False:
            lines.append('#[serde(deny_unknown_fields)]')
        lines.append(f'pub struct {name} {{')
        required = set(schema.get('required', []))
        defaults = []
        for field, prop in schema['properties'].items():
            if not re.fullmatch(r'[a-z][a-z0-9_]*', field):
                raise ValueError(f'Invalid field name: {field}')
            ty = rust_type(prop, schemas)
            optional = field not in required
            is_null = nullable(prop) is not None
            if not optional and is_null:
                lines.append('    #[serde(deserialize_with = "required_nullable")]')
            if optional and not prop.get('x-rust-default'):
                if not prop.get('x-rust-optional'):
                    raise ValueError(f'{name}.{field} needs explicit optional policy')
                if not is_null:
                    ty = f'Option<{ty}>'
                lines.append('    #[serde(default)]')
                if not is_null:
                    lines.append('    #[serde(skip_serializing_if = "Option::is_none")]')
            if prop.get('x-rust-default'):
                if not optional or 'default' not in prop:
                    raise ValueError('A Rust default requires optional field and schema default')
                func = re.sub(r'(?<!^)(?=[A-Z])', '_', name).lower() + '_' + field + '_default'
                lines.append(f'    #[serde(default = "{func}")]')
                defaults += [f'fn {func}() -> {ty} {{ {literal(prop["default"], ty)} }}']
            if prop.get('x-rust-omit-false'):
                if ty != 'bool' or prop.get('default') is not False:
                    raise ValueError('omit-false requires a default false boolean')
                lines.append('    #[serde(skip_serializing_if = "std::ops::Not::not")]')
            lines.append(f'    pub {field}: {ty},')
        lines += ['}', *defaults]
        if schema.get('x-rust-redacted-debug'):
            lines += [f'impl std::fmt::Debug for {name} {{',
                      "    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {",
                      f'        f.debug_struct("{name}").finish_non_exhaustive()', '    }', '}']
        lines.append('')
    rendered = subprocess.run(['rustfmt', '--edition', '2024', '--emit', 'stdout'],
                              input='\n'.join(lines), text=True, capture_output=True, check=True)
    return rendered.stdout


def snake(name):
    return re.sub(r'(?<=[a-z0-9])(?=[A-Z])', '_', name).replace('-', '_').lower()


def render_requests(doc):
    """Finite request profile. Unsupported media/parameters fail closed."""
    def resolve(value):
        if '$ref' in value:
            result = doc
            for part in value['$ref'].removeprefix('#/').split('/'):
                result = result[part]
            return result
        return value

    lines = ['//! Generated from api/openapi.json. Do not edit.',
             'use crate::{Client, Error, RangeChunk, EventStream, models::*};',
             "pub(crate) fn known_problem_code(code: &str) -> Option<&'static str> { match code {"]
    codes = doc['components']['schemas']['ProblemBody']['properties']['code']['enum']
    for code in codes:
        lines.append(f'{json.dumps(code)} => Some({json.dumps(code)}),')
    lines += ['_ => None,', '}}']
    for path, methods in doc['paths'].items():
        for method, op in methods.items():
            if method not in ('get', 'post', 'put', 'delete'):
                raise ValueError('Unsupported request method')
            name = op['operationId'][0].upper() + op['operationId'][1:]
            if not re.fullmatch(r'[A-Z][A-Za-z0-9]*', name):
                raise ValueError('Invalid operation name')
            params = [resolve(p) for p in op.get('parameters', [])]
            fields, setup = [], []
            for p in params:
                field = snake(p['name'])
                if not re.fullmatch(r'[a-z][a-z0-9_]*', field):
                    raise ValueError('Invalid parameter name')
                ty = resolve(p['schema']).get('type')
                ty = {'string': "&'a str", 'integer': 'u64'}[ty]
                required = p.get('required', False)
                fields.append(f"pub {field}: {ty if required else f'Option<{ty}>'},")
                val = f'args.{field}' if required else 'value'
                if p['in'] == 'header':
                    header_val = val if ty == "&'a str" else f'&{val}.to_string()'
                    action = f'request = self.header(request, {json.dumps(p["name"])}, {header_val})?;'
                elif p['in'] == 'query':
                    query_val = val if ty == "&'a str" else f'&{val}.to_string()'
                    action = f'request = self.query(request, {json.dumps(p["name"])}, {query_val})?;'
                elif p['in'] == 'path':
                    action = ''
                else:
                    raise ValueError('Unsupported parameter location')
                if action:
                    setup.append(action if required else f'if let Some(value) = args.{field} {{ {action} }}')
            body = op.get('requestBody', {}).get('content', {})
            if body:
                if len(body) != 1:
                    raise ValueError('Ambiguous request media')
                media, definition = next(iter(body.items()))
                if media == 'application/json':
                    ty = rust_type(definition['schema'], doc['components']['schemas'])
                    fields.append(f"pub body: &'a {ty},")
                    setup.append('request = self.json_body(request, args.body)?;')
                elif media == 'application/octet-stream':
                    fields.append("pub body: &'a [u8],")
                    setup.append('request = self.binary_body(request, args.body)?;')
                else:
                    raise ValueError('Unsupported request media')
            successes = [(code, resolve(r)) for code, r in op['responses'].items() if code.startswith('2')]
            if len(successes) != 1:
                raise ValueError('Exactly one success status is required')
            code, response = successes[0]
            content = response.get('content', {})
            if not content:
                ret, call = '()', 'empty'
            elif len(content) != 1:
                raise ValueError('Ambiguous response media')
            elif 'application/json' in content:
                ret = rust_type(content['application/json']['schema'], doc['components']['schemas'])
                call = 'json'
            elif 'application/octet-stream' in content:
                ret, call = 'RangeChunk', 'binary'
            elif 'text/event-stream' in content:
                ret, call = 'EventStream', 'events'
            else:
                raise ValueError('Unsupported response media')
            segments = []
            for part in path.lstrip('/').split('/'):
                segments.append('args.' + part[1:-1] if part.startswith('{') else json.dumps(part))
            lines += [f"pub struct {name}<'a> {{", *fields, '}',
                      f"impl std::fmt::Debug for {name}<'_> {{",
                      "fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {",
                      f'f.debug_struct("{name}").finish_non_exhaustive()', '}', '}',
                      'impl Client {',
                      f"pub async fn {snake(op['operationId'])}(&self, args: {name}<'_>) -> Result<{ret}, Error> {{",
                      f'let mut request = self.request(reqwest::Method::{method.upper()}, &[{", ".join(segments)}])?;',
                      *setup]
            # GET with no query/header/body still uses the same transport without an unused mut.
            if not setup:
                lines[-1] = lines[-1].replace('let mut request', 'let request')
            if call == 'binary':
                prefix = 'output' if 'X-Output-Offset' in response['headers'] else 'file'
                lines.append(f'self.binary(request, {code}, "{prefix}", args.offset.unwrap_or(0), args.limit.unwrap_or(32768)).await')
            else:
                lines.append(f'self.{call}(request, {code}).await')
            lines += ['}', '}']
    return subprocess.run(['rustfmt', '--edition', '2024', '--emit', 'stdout'],
                          input='\n'.join(lines), text=True, capture_output=True, check=True).stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    doc = json.loads(SPEC.read_text())
    rendered = render(doc)
    outputs = {OUTPUT: rendered, CLIENT / 'models.rs': rendered,
               CLIENT / 'requests.rs': render_requests(doc)}
    for output, rendered in outputs.items():
        if args.check:
            if not output.exists() or output.read_text() != rendered:
                raise SystemExit('Generated API code is stale: run python3 scripts/generate_api.py')
        else:
            output.write_text(rendered)
    if args.check:
        print('Generated API models and requests match OpenAPI.')


if __name__ == '__main__':
    main()
