<script lang="ts">
  import {
    SvelteFlow,
    Background,
    Controls,
    MiniMap,
    addEdge,
    type Node,
    type Edge,
    type Connection
  } from '@xyflow/svelte';
  import '@xyflow/svelte/dist/style.css';
  import AgentNode from '$lib/AgentNode.svelte';
  import ConfigPanel from '$lib/ConfigPanel.svelte';
  import RunPanel from '$lib/RunPanel.svelte';
  import { NODE_DEFS, type NodeKind, type RunResponse, type Workflow } from '$lib/types';
  import { api } from '$lib/api';
  import { onMount } from 'svelte';

  // Every kind renders with the same custom component.
  const nodeTypes = Object.fromEntries(
    Object.keys(NODE_DEFS).map((k) => [k, AgentNode])
  ) as Record<string, typeof AgentNode>;

  let nodes = $state.raw<Node[]>([]);
  let edges = $state.raw<Edge[]>([]);
  let wfName = $state('Untitled agent');
  let wfId = $state('');
  let saved = $state<Workflow[]>([]);
  let selectedId = $state<string | null>(null);
  let runResult = $state<RunResponse | null>(null);
  let running = $state(false);
  let triggerInput = $state('Hello from Optimimer');
  let toast = $state('');

  let idc = 1;
  const newId = (k: string) => `${k}_${idc++}`;

  const selectedNode = $derived(nodes.find((n) => n.id === selectedId) ?? null);

  function seedStarter() {
    // A small ready-to-run agent so the canvas is never blank.
    const t = newId('trigger');
    const l = newId('llm');
    const o = newId('output');
    nodes = [
      { id: t, type: 'trigger', position: { x: 40, y: 160 }, data: { ...NODE_DEFS.trigger.defaults } },
      {
        id: l,
        type: 'llm',
        position: { x: 320, y: 140 },
        data: { ...NODE_DEFS.llm.defaults, prompt: 'Rewrite this more cheerfully: {{input}}' }
      },
      {
        id: o,
        type: 'output',
        position: { x: 620, y: 160 },
        data: { ...NODE_DEFS.output.defaults, value: '{{' + l + '.text}}' }
      }
    ];
    edges = [
      { id: newId('e'), source: t, target: l },
      { id: newId('e'), source: l, target: o }
    ];
  }

  function addNode(kind: NodeKind) {
    const id = newId(kind);
    const offset = nodes.length * 24;
    nodes = [
      ...nodes,
      {
        id,
        type: kind,
        position: { x: 160 + offset, y: 120 + offset },
        data: { ...NODE_DEFS[kind].defaults }
      }
    ];
    selectedId = id;
  }

  function onconnect(conn: Connection) {
    edges = addEdge(conn, edges);
  }

  function updateNodeData(id: string, patch: Record<string, unknown>) {
    nodes = nodes.map((n) => (n.id === id ? { ...n, data: { ...n.data, ...patch } } : n));
  }

  function deleteSelected() {
    if (!selectedId) return;
    nodes = nodes.filter((n) => n.id !== selectedId);
    edges = edges.filter((e) => e.source !== selectedId && e.target !== selectedId);
    selectedId = null;
  }

  function flash(msg: string) {
    toast = msg;
    setTimeout(() => (toast = ''), 2200);
  }

  async function refreshList() {
    try {
      saved = await api.list();
    } catch (e) {
      console.error(e);
    }
  }

  async function save() {
    try {
      const wf = wfId
        ? await api.update(wfId, wfName, nodes, edges)
        : await api.create(wfName, nodes, edges);
      wfId = wf.id;
      flash('Saved ✓');
      refreshList();
    } catch (e) {
      flash('Save failed: ' + (e as Error).message);
    }
  }

  async function load(id: string) {
    const wf = await api.get(id);
    wfId = wf.id;
    wfName = wf.name;
    nodes = wf.nodes;
    edges = wf.edges;
    selectedId = null;
    runResult = null;
    // keep id counter ahead of loaded ids
    for (const n of wf.nodes) {
      const num = parseInt(n.id.split('_').pop() ?? '0', 10);
      if (num >= idc) idc = num + 1;
    }
  }

  function newAgent() {
    wfId = '';
    wfName = 'Untitled agent';
    runResult = null;
    selectedId = null;
    seedStarter();
  }

  async function run() {
    running = true;
    runResult = null;
    let input: unknown = triggerInput;
    try {
      input = JSON.parse(triggerInput);
    } catch {
      /* treat as plain string */
    }
    try {
      runResult = await api.run(wfId, { name: wfName, nodes, edges }, input);
    } catch (e) {
      flash('Run failed: ' + (e as Error).message);
    } finally {
      running = false;
    }
  }

  onMount(() => {
    seedStarter();
    refreshList();
  });
</script>

<div class="app">
  <!-- Left: node palette + saved agents -->
  <aside class="palette">
    <div class="brand">⟁ Optimimer</div>
    <div class="section-title">Blocks</div>
    {#each Object.values(NODE_DEFS) as def}
      <button class="palette-item" style:--accent={def.color} onclick={() => addNode(def.kind)}>
        <span class="pi-icon">{def.icon}</span>
        <span>
          <span class="pi-label">{def.label}</span>
          <span class="pi-hint">{def.hint}</span>
        </span>
      </button>
    {/each}

    <div class="section-title">Saved agents</div>
    <div class="saved-list">
      {#each saved as w}
        <button class="saved-item" class:active={w.id === wfId} onclick={() => load(w.id)}>
          {w.name}
        </button>
      {:else}
        <div class="empty">No saved agents yet.</div>
      {/each}
    </div>
  </aside>

  <!-- Center: toolbar + canvas -->
  <main class="canvas-wrap">
    <header class="toolbar">
      <input class="name-input" bind:value={wfName} />
      <div class="spacer"></div>
      <button class="btn ghost" onclick={newAgent}>New</button>
      <button class="btn ghost" onclick={save}>Save</button>
      <button class="btn primary" onclick={run} disabled={running}>
        {running ? 'Running…' : '▶ Run'}
      </button>
    </header>

    <div class="canvas">
      <SvelteFlow
        bind:nodes
        bind:edges
        {nodeTypes}
        colorMode="dark"
        {onconnect}
        onnodeclick={(e) => (selectedId = (e.node as Node).id)}
        onpaneclick={() => (selectedId = null)}
        fitView
      >
        <Background gap={20} />
        <Controls />
        <MiniMap pannable zoomable />
      </SvelteFlow>

      {#if toast}
        <div class="toast">{toast}</div>
      {/if}
    </div>
  </main>

  <!-- Right: config or run results -->
  <aside class="inspector">
    {#if selectedNode}
      <ConfigPanel
        node={selectedNode}
        onchange={(patch) => updateNodeData(selectedNode.id, patch)}
        ondelete={deleteSelected}
      />
    {:else}
      <RunPanel bind:triggerInput {runResult} {running} />
    {/if}
  </aside>
</div>

<style>
  .app {
    display: grid;
    grid-template-columns: 260px 1fr 320px;
    height: 100vh;
    overflow: hidden;
  }
  .palette,
  .inspector {
    background: #0d111b;
    border-right: 1px solid #1c2333;
    padding: 14px;
    overflow-y: auto;
  }
  .inspector {
    border-right: none;
    border-left: 1px solid #1c2333;
  }
  .brand {
    font-weight: 700;
    font-size: 18px;
    margin-bottom: 16px;
    letter-spacing: 0.5px;
  }
  .section-title {
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 1px;
    color: #6b7488;
    margin: 16px 0 8px;
  }
  .palette-item {
    display: flex;
    gap: 10px;
    align-items: flex-start;
    width: 100%;
    text-align: left;
    background: #141a28;
    border: 1px solid #232c40;
    border-left: 3px solid var(--accent);
    border-radius: 8px;
    padding: 9px 10px;
    margin-bottom: 7px;
    color: inherit;
    cursor: pointer;
  }
  .palette-item:hover {
    background: #1a2233;
  }
  .pi-icon {
    font-size: 16px;
  }
  .pi-label {
    display: block;
    font-weight: 600;
    font-size: 13px;
  }
  .pi-hint {
    display: block;
    font-size: 11px;
    color: #6b7488;
    line-height: 1.3;
  }
  .saved-item {
    display: block;
    width: 100%;
    text-align: left;
    background: transparent;
    border: 1px solid transparent;
    border-radius: 6px;
    padding: 7px 9px;
    color: #c2c9d6;
    cursor: pointer;
    font-size: 13px;
  }
  .saved-item:hover {
    background: #141a28;
  }
  .saved-item.active {
    background: #1a2233;
    border-color: #2f3a52;
  }
  .empty {
    font-size: 12px;
    color: #586075;
  }
  .canvas-wrap {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  .toolbar {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 10px 14px;
    border-bottom: 1px solid #1c2333;
    background: #0d111b;
  }
  .name-input {
    background: #141a28;
    border: 1px solid #232c40;
    border-radius: 6px;
    padding: 7px 10px;
    color: inherit;
    font-size: 14px;
    font-weight: 600;
    min-width: 220px;
  }
  .spacer {
    flex: 1;
  }
  .btn {
    border-radius: 6px;
    padding: 7px 14px;
    font-size: 13px;
    font-weight: 600;
    cursor: pointer;
    border: 1px solid #2f3a52;
    background: #141a28;
    color: inherit;
  }
  .btn.ghost:hover {
    background: #1a2233;
  }
  .btn.primary {
    background: #6d4bff;
    border-color: #6d4bff;
    color: white;
  }
  .btn.primary:disabled {
    opacity: 0.6;
    cursor: default;
  }
  .canvas {
    position: relative;
    flex: 1;
    min-height: 0;
  }
  .toast {
    position: absolute;
    bottom: 16px;
    left: 50%;
    transform: translateX(-50%);
    background: #1a2233;
    border: 1px solid #2f3a52;
    padding: 8px 16px;
    border-radius: 8px;
    font-size: 13px;
  }
</style>
