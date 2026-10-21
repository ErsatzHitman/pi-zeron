import assert from 'node:assert/strict';
import {createInterface} from 'node:readline';
import {pathToFileURL} from 'node:url';

if (process.argv[2] === 'server') {
  createInterface({input: process.stdin}).on('line', line => {
    const msg = JSON.parse(line);
    let result;
    if (msg.method === 'initialize') result = {protocolVersion: '2024-11-05', capabilities: {tools:{}}, serverInfo: {name:'test',version:'1'}};
    else if (msg.method === 'tools/list') result = {tools:['whoami','create_chat','create_chats','read_chat','send_message','wait_for_turn'].map(name => ({name,description:'Zeron delegation',inputSchema:{type:'object',properties:{mode:{type:'string'}}}}))};
    else if (msg.method === 'tools/call') {
      if (msg.params.arguments.mode === 'hang') return;
      if (msg.params.arguments.mode === 'crash') process.exit(1);
      result = {content:[{type:'text',text:process.env.ZERON_CHAT_ID}], isError:msg.params.arguments.mode === 'error'};
    } else return;
    process.stdout.write(JSON.stringify({jsonrpc:'2.0', id:msg.id, result})+'\n');
  });
} else {
  // A Windows path (D:\...) is not an ESM specifier; a file URL is.
  const {default: extension, callTimeoutMs} = await import(pathToFileURL(process.argv[2]).href);
  // A call outlives the longest wait it asks for, a batch's included.
  assert.equal(callTimeoutMs('initialize', {}), 15_000);
  assert.equal(callTimeoutMs('tools/call', {name: 'read_chat', arguments: {}}), 660_000);
  assert.equal(callTimeoutMs('tools/call', {arguments: {timeout_secs: 3600}}), 3_630_000);
  assert.equal(callTimeoutMs('tools/call', {arguments: {requests: [{timeout_secs: 60}, {timeout_secs: 1800}]}}), 1_830_000);
  // A plain chat defers the tools it rarely needs; a tree member keeps all; a
  // btw chat's allowlist registers neither them nor tool_search, so none is named.
  for (const [chat, role, allowlist] of [['first', undefined], ['second', 'member'], ['btw-third', undefined, true]]) {
    if (role) process.env.ZERON_ROLE = role; else delete process.env.ZERON_ROLE;
    process.env.ZERON_PI_MCP = JSON.stringify({name:'zeron',command:process.execPath,args:[process.argv[1],'server'],env:{ZERON_CHAT_ID:chat}});
    const handlers = {}, tools = {};
    let active = ['read'];
    extension({
      on:(event,fn)=>handlers[event]=fn,
      registerTool:tool=>{ tools[tool.name]=tool; if (!tool.exposure) active.push(tool.name); },
      getActiveTools:()=>[...active],
      setActiveTools:names=>{ active = names; },
      getAllTools:()=>allowlist ? [] : [{name:'tool_search'}, ...Object.values(tools)],
    });
    try {
      await handlers.session_start();
      const options = {sections: {}};
      handlers.before_agent_start({systemPromptOptions: options});
      if (role) {
        assert.equal(tools.zeron_whoami.exposure, undefined);
        assert.ok(!active.includes('tool_search'));
        assert.equal(options.sections.zeron_tools, undefined);
      } else if (allowlist) {
        assert.ok(!active.includes('tool_search'));
        assert.equal(options.sections.zeron_tools, undefined);
      } else {
        assert.equal(tools.zeron_whoami.exposure, 'deferred');
        assert.equal(tools.zeron_create_chat.exposure, undefined);
        assert.ok(active.includes('tool_search') && !active.includes('zeron_whoami'));
        assert.match(options.sections.zeron_tools, /tool_search/);
        assert.match(options.sections.zeron_tools, /zeron_whoami: /);
      }
      for (const name of ['create_chat','create_chats','read_chat','send_message','wait_for_turn']) {
        assert.equal((await tools[`zeron_${name}`].execute('delegation',{})).content[0].text, chat);
      }
      const tool = tools.zeron_whoami;
      assert.equal(tool.parameters.type,'object');
      const result = await tool.execute('call',{});
      assert.equal(result.content[0].text,chat);
      await assert.rejects(tool.execute('call',{mode:'error'}), new RegExp(chat));
      const controller = new AbortController();
      const waiting = tool.execute('call',{mode:'hang'},controller.signal);
      controller.abort();
      await assert.rejects(waiting,/cancelled/);
      await assert.rejects(tool.execute('call',{mode:'crash'}),/exited/);
      assert.equal(handlers.message_end, undefined, "native RPC reports provider failures directly");
      // Restarting the session must not let an old child's exit fail new RPCs.
      await handlers.session_start();
      assert.equal((await tools.zeron_whoami.execute('call',{})).content[0].text,chat);
    } finally { handlers.session_shutdown(); }
    await assert.rejects(tools.zeron_whoami.execute('call',{}),/not connected/);
  }
}
