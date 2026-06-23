<script lang="ts">
  import type { Node } from '@xyflow/svelte';
  import { NODE_DEFS, type NodeKind } from './types';

  let {
    node,
    onchange,
    ondelete
  }: {
    node: Node;
    onchange: (patch: Record<string, unknown>) => void;
    ondelete: () => void;
  } = $props();

  const def = $derived(NODE_DEFS[(node.type ?? 'trigger') as NodeKind]);

  function set(key: string, value: string) {
    onchange({ [key]: value });
  }
</script>

<div class="panel">
  <div class="header" style:--accent={def.color}>
    <span class="icon">{def.icon}</span>
    <div>
      <div class="title">{def.label}</div>
      <code class="id">{node.id}</code>
    </div>
  </div>

  <p class="hint">{def.hint}</p>

  {#each def.fields as f (f.key)}
    <label class="field">
      <span class="flabel">{f.label}</span>
      {#if f.type === 'textarea'}
        <textarea
          rows="4"
          placeholder={f.placeholder ?? ''}
          value={(node.data?.[f.key] as string) ?? ''}
          oninput={(e) => set(f.key, e.currentTarget.value)}
        ></textarea>
      {:else if f.type === 'select'}
        <select
          value={(node.data?.[f.key] as string) ?? ''}
          onchange={(e) => set(f.key, e.currentTarget.value)}
        >
          {#each f.options ?? [] as opt}
            <option value={opt}>{opt}</option>
          {/each}
        </select>
      {:else}
        <input
          type="text"
          placeholder={f.placeholder ?? ''}
          value={(node.data?.[f.key] as string) ?? ''}
          oninput={(e) => set(f.key, e.currentTarget.value)}
        />
      {/if}
    </label>
  {/each}

  <button class="delete" onclick={ondelete}>Delete block</button>
</div>

<style>
  .header {
    display: flex;
    align-items: center;
    gap: 10px;
    padding-bottom: 12px;
    border-bottom: 1px solid #1c2333;
  }
  .icon {
    font-size: 22px;
  }
  .title {
    font-weight: 700;
  }
  .id {
    font-size: 11px;
    color: #6b7488;
  }
  .hint {
    font-size: 12px;
    color: #8a94ab;
    margin: 12px 0;
    line-height: 1.4;
  }
  .field {
    display: block;
    margin-bottom: 12px;
  }
  .flabel {
    display: block;
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: #6b7488;
    margin-bottom: 5px;
  }
  input,
  textarea,
  select {
    width: 100%;
    box-sizing: border-box;
    background: #141a28;
    border: 1px solid #232c40;
    border-radius: 6px;
    padding: 8px 10px;
    color: #e6e9ef;
    font-size: 13px;
    font-family: inherit;
    resize: vertical;
  }
  input:focus,
  textarea:focus,
  select:focus {
    outline: none;
    border-color: #6d4bff;
  }
  .delete {
    width: 100%;
    margin-top: 8px;
    background: transparent;
    border: 1px solid #4a2230;
    color: #ef6a7d;
    border-radius: 6px;
    padding: 8px;
    font-size: 13px;
    cursor: pointer;
  }
  .delete:hover {
    background: #2a1419;
  }
</style>
