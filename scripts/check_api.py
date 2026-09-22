#!/usr/bin/env python3
"""Validate OpenAPI and optional actual router exchanges supplied on stdin."""
import argparse
import base64
import json
from pathlib import Path
import re
import sys
from urllib.parse import parse_qsl, urlsplit
from jsonschema import Draft202012Validator, FormatChecker
from openapi_spec_validator import validate

ROOT = Path(__file__).resolve().parents[1]


def resolve(spec, value):
    while '$ref' in value:
        pointer = value['$ref']
        if not pointer.startswith('#/'):
            raise ValueError('Only local contract references are permitted')
        value = spec
        for part in pointer[2:].split('/'):
            value = value[part.replace('~1', '/').replace('~0', '~')]
    return value


def check_schema(spec, schema, value):
    # Keep the complete local components namespace when validating a subschema.
    bundle = {'components': spec['components'], **schema}
    Draft202012Validator(bundle, format_checker=FormatChecker()).validate(value)


def header_value(spec, schema, value):
    schema = resolve(spec, schema)
    if schema.get('type') == 'integer':
        assert re.fullmatch(r'-?[0-9]+', value), f'Invalid integer header: {value}'
        return int(value)
    if schema.get('type') == 'boolean':
        assert value in ('true', 'false'), f'Invalid boolean header: {value}'
        return value == 'true'
    return value


def operation(spec, method, uri):
    parts = urlsplit(uri)
    for path, item in spec['paths'].items():
        names = re.findall(r'\{([^}]+)\}', path)
        pattern = re.escape(path)
        for name in names:
            pattern = pattern.replace(re.escape('{'+name+'}'), '([^/]+)')
        match = re.fullmatch(pattern, parts.path)
        if match and method.lower() in item:
            return item[method.lower()], dict(zip(names, match.groups())), parts
    raise AssertionError(f'Undocumented route: {method} {uri}')


def check_exchange(spec, exchange):
    op, path_values, uri = operation(spec, exchange['method'], exchange['uri'])
    status = str(exchange['status'])
    assert status in op['responses'], f'{op["operationId"]}: undocumented status {status}'
    success = 200 <= int(status) < 300
    if success:
        query = dict(parse_qsl(uri.query, keep_blank_values=True))
        headers = exchange['request_headers']
        for param in op.get('parameters', []):
            param = resolve(spec, param)
            name = param['name']
            values = {'path': path_values, 'query': query, 'header': headers}[param['in']]
            key = name.lower() if param['in'] == 'header' else name
            if param.get('required'):
                assert key in values, f'Missing {name}'
            if key in values:
                check_schema(spec, param['schema'], header_value(spec, param['schema'], values[key]))
        if 'requestBody' in op:
            content = op['requestBody']['content']
            media = headers['content-type'].split(';')[0]
            assert media in content
            if media == 'application/json':
                check_schema(spec, content[media]['schema'], exchange['request_json'])
    response = resolve(spec, op['responses'][status])
    headers = exchange['response_headers']
    for name, param in response.get('headers', {}).items():
        if param.get('required'):
            assert name.lower() in headers, f'{op["operationId"]}: missing {name}'
        if name.lower() in headers:
            check_schema(spec, param['schema'], header_value(spec, param['schema'], headers[name.lower()]))
    content = response.get('content', {})
    if not content:
        assert exchange['response_bytes'] == 0
    else:
        media = headers['content-type'].split(';')[0]
        assert media in content, f'Unexpected response content type {media}'
        if media.endswith('json'):
            check_schema(spec, content[media]['schema'], exchange['response_json'])
            if media == 'application/problem+json':
                assert exchange['response_json']['status'] == int(status)
        elif media == 'text/event-stream':
            events = []
            for frame in exchange['response_text'].split('\n\n'):
                fields = {}
                for line in frame.splitlines():
                    if line and not line.startswith(':'):
                        key, value = line.split(':', 1)
                        fields[key] = value.lstrip(' ')
                if not fields:
                    continue
                event = fields['event']
                data = json.loads(fields['data'])
                check_schema(spec, op['x-sse-events'][event], data)
                if event == 'output':
                    raw = base64.b64decode(data['data_base64'], validate=True)
                    assert len(raw) <= 32768
                    assert data['next_offset'] == data['offset'] + len(raw)
                    assert data['next_offset'] <= data['stored'] <= data['seen']
                    assert data['at_end'] == (data['next_offset'] == data['stored'])
                assert ('id' in fields) == (event in ('output', 'end'))
                events.append(event)
            assert events and events[-1] in ('end', 'gap', 'error'), 'Test stream needs a terminal event'
        else:
            assert media == 'application/octet-stream'
            assert exchange['response_bytes'] <= 32768
            prefix = 'x-output-' if 'x-output-offset' in headers else 'x-file-'
            offset, next_offset, size = (int(headers[prefix+field]) for field in ('offset', 'next-offset', 'size'))
            assert next_offset == offset + exchange['response_bytes'] <= size
            assert (headers[prefix+'eof'] == 'true') == (next_offset == size)
    return op['operationId'] if success else None


def check_registered_routes(spec):
    # The API registers literal paths and simple Axum method chains. Refuse
    # unfamiliar registration syntax instead of claiming it was inspected.
    registered = set()
    for source in (ROOT/'crates/sandbox-api/src').rglob('*.rs'):
        text = source.read_text()
        matches = list(re.finditer(r'\.route\(\s*"([^"\n]+)"\s*,', text))
        assert len(matches) == text.count('.route('), f'Unrecognized route registration in {source}'
        for match in matches:
            depth = 1
            end = match.end()
            while depth and end < len(text):
                depth += (text[end] == '(') - (text[end] == ')')
                end += 1
            assert depth == 0, f'Unbalanced route registration in {source}'
            methods = re.findall(r'\b(get|post|put|delete|patch|head|options)\(', text[match.end():end-1])
            assert methods, f'Unrecognized HTTP method registration in {source}'
            path = re.sub(r'\{[^}]+\}', '{}', match[1])
            registered.update((path, method) for method in methods)
    declared = {(re.sub(r'\{[^}]+\}', '{}', path), method)
                for path, item in spec['paths'].items() for method in item}
    assert registered == declared, f'Route drift: undocumented={registered-declared}, unimplemented={declared-registered}'


def check_profile(spec):
    # Fail closed for external references and unsupported code-generation composition.
    def walk(node):
        if isinstance(node, dict):
            if '$ref' in node:
                resolve(spec, node)
            for child in node.values():
                walk(child)
        elif isinstance(node, list):
            for child in node:
                walk(child)
    walk(spec)
    assert spec['security'] == [{'ProjectBearer': []}]
    names = []
    for path, item in spec['paths'].items():
        assert path.startswith('/v1/')
        for op in item.values():
            names.append(op['operationId'])
            assert 'security' not in op, 'Do not override project authentication'
    assert len(set(names)) == len(names)
    return set(names)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--exchanges', action='store_true')
    args = parser.parse_args()
    spec = json.loads((ROOT/'api/openapi.json').read_text())
    expected = check_profile(spec)
    check_registered_routes(spec)
    validate(spec)
    if args.exchanges:
        exchanges = json.load(sys.stdin)
        covered = {check_exchange(spec, e) for e in exchanges} - {None}
        assert covered == expected, f'Missing successful route evidence: {expected-covered}'
        print(f'Validated {len(exchanges)} actual exchanges across {len(covered)} operations.')
    else:
        print(f'OpenAPI is valid: {len(expected)} implemented operations.')


if __name__ == '__main__':
    main()
