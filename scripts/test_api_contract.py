"""Regression checks for contract restrictions and serialization generation."""
import copy
import json
from pathlib import Path
import unittest
from jsonschema import ValidationError
from generate_api import render, rust_type
from check_api import check_schema, check_profile, check_exchange, check_registered_routes

ROOT = Path(__file__).resolve().parents[1]
SPEC = json.loads((ROOT/'api/openapi.json').read_text())


class ContractTests(unittest.TestCase):
    def test_request_bounds_and_closed_command_shape(self):
        schema={'$ref':'#/components/schemas/CommandInput'}
        valid={'argv':['/bin/true'],'deadline_unix_ms':1}
        check_schema(SPEC,schema,valid)
        for extra in [{'argv':[]},{'argv':['a\0b']},{'cwd':'relative'}, {'env':{'BAD=KEY':'x'}}, {'output_limit':0}, {'output_limit':10485761}, {'shell':True}]:
            with self.subTest(extra=extra),self.assertRaises(ValidationError):
                check_schema(SPEC,schema,{**valid,**extra})

    def test_id_and_size_contracts(self):
        for name,bad in [('SandboxId','sb_01996110-7c00-7000-8000-000000000000'),('SandboxId','sbx_01996110-7c00-4000-8000-000000000000'),('RequestedResources',{'vcpu':1,'memory_mib':127,'disk_mib':64})]:
            with self.subTest(name=name),self.assertRaises(ValidationError):
                check_schema(SPEC,{'$ref':'#/components/schemas/'+name},bad)

    def test_workspace_path_shape_excludes_ambiguous_names(self):
        schema={'$ref':'#/components/schemas/WorkspacePath'}
        check_schema(SPEC,schema,'dir/file.bin')
        for value in ['', '/absolute', 'dir//file', '../file', 'dir/../file', './file', '.hudson-transfers/file', 'dir\\file', 'bad\x00name']:
            with self.subTest(value=value),self.assertRaises(ValidationError):
                check_schema(SPEC,schema,value)

    def test_null_and_omitted_are_deliberately_different(self):
        check_schema(SPEC,{'$ref':'#/components/schemas/DestroyRequest'},{'correlation_id':None})
        with self.assertRaises(ValidationError):
            check_schema(SPEC,{'$ref':'#/components/schemas/SandboxList'},{'items':[]})
        check_schema(SPEC,{'$ref':'#/components/schemas/SandboxList'},{'items':[],'next_cursor':None})

    def test_route_inventory_detects_omissions_and_invented_endpoints(self):
        check_registered_routes(SPEC)
        for mutate in [lambda s:s['paths'].pop('/v1/operations'),lambda s:s['paths'].update({'/v1/not-implemented':{'get':{}}})]:
            spec=copy.deepcopy(SPEC);mutate(spec)
            with self.assertRaises(AssertionError):
                check_registered_routes(spec)

    def test_external_refs_and_auth_overrides_fail(self):
        for mutate in [lambda s:s['components']['schemas'].update({'Remote':{'$ref':'https://example.invalid/schema'}}), lambda s:s['paths']['/v1/sandboxes']['get'].update({'security':[]})]:
            spec=copy.deepcopy(SPEC);mutate(spec)
            with self.assertRaises((ValueError,AssertionError)):
                check_profile(spec)

    def test_generator_fails_on_unsupported_composition(self):
        for shape in [{'oneOf':[{'type':'string'},{'type':'integer'}]}, {'anyOf':[{'type':'string'},{'type':'integer'}]}, {'type':['string','integer']}, {'type':'object','properties':{'secret':{'type':'string'}}}]:
            with self.subTest(shape=shape),self.assertRaises(ValueError):
                rust_type(shape,SPEC['components']['schemas'])

    def test_wire_generation_is_deterministic_and_redacts_commands(self):
        first=render(SPEC)
        self.assertEqual(first,render(SPEC))
        self.assertIn('f.debug_struct("CommandInput").finish_non_exhaustive()',first)
        self.assertIn('pub next_cursor: Option<String>',first)
        self.assertIn('command_input_output_limit_default',first)

    def test_binary_header_positions_must_match_delivered_bytes(self):
        e={'method':'GET','uri':'/v1/operations/op_01996110-7c00-7000-8000-000000000000/outputs/stdout','status':200,'request_headers':{},'response_headers':{'content-type':'application/octet-stream','cache-control':'no-store','x-output-offset':'0','x-output-next-offset':'2','x-output-size':'4','x-output-seen':'4','x-output-eof':'false','x-output-truncated':'false','x-output-simulated':'true','content-disposition':'attachment; filename=stdout.bin','x-content-type-options':'nosniff'},'response_bytes':2}
        self.assertEqual(check_exchange(SPEC,e),'readOutput')
        for mutate in [lambda x:x.update(response_bytes=3),lambda x:x['response_headers'].update({'x-output-eof':'true'})]:
            bad=copy.deepcopy(e);mutate(bad)
            with self.assertRaises(AssertionError):
                check_exchange(SPEC,bad)

    def test_response_checker_detects_wire_drift(self):
        e={'method':'GET','uri':'/v1/sandboxes','status':200,'request_headers':{},'response_headers':{'content-type':'application/json','cache-control':'no-store'},'response_bytes':31,'response_json':{'items':[],'next_cursor':None}}
        self.assertEqual(check_exchange(SPEC,e),'listSandboxes')
        for mutate in [lambda x:x['response_headers'].pop('cache-control'),lambda x:x['response_json'].pop('next_cursor'),lambda x:x.update(status=201)]:
            bad=copy.deepcopy(e);mutate(bad)
            with self.assertRaises((AssertionError,ValidationError)):
                check_exchange(SPEC,bad)


if __name__=='__main__':
    unittest.main()
