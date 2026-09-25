<script lang="ts">
  import { Handle, Position, type NodeProps } from '@xyflow/svelte';
  import { NODE_DEFS, type NodeKind } from './types';

  // Svelte Flow passes these as props to custom nodes.
  let { id, type, data, selected }: NodeProps = $props();

  const kind = (type ?? 'trigger') as NodeKind;
  const def = $derived(NODE_DEFS[kind] ?? NODE_DEFS.trigger);
  const label = $derived((data?.label as string) ?? def.label);

  // A short, type-specific subtitle so nodes are readable at a glance.
  const subtitle = $derived.by(() => {
    if (kind === 'llm') return (data?.model as string) ?? '';
    if (kind === 'jev') return `${data?.kind ?? 'choice'}: ${(data?.instructions as string) ?? ''}`;
    if (kind === 'http') return `${data?.method ?? 'GET'} ${(data?.url as string) ?? ''}`;
    if (kind === 'condition') return `${data?.left ?? ''} ${data?.op ?? ''} ${data?.right ?? ''}`;
    return def.hint;
  });
</script>

<div
  class="agent-node"
  class:selected
  style:--accent={def.color}
  title={def.hint}
>
  {#if kind !== 'trigger'}
    <Handle type="target" position={Position.Left} />
  {/if}

  <div class="head">
    <span class="icon">{def.icon}</span>
    <span class="label">{label}</span>
  </div>
  <div class="sub">{subtitle}</div>

  {#if kind === 'condition'}
    <div class="branch-labels">
      <span class="true">true</span>
      <span class="false">false</span>
    </div>
    <Handle type="source" position={Position.Right} id="true" style="top: 38%" />
    <Handle type="source" position={Position.Right} id="false" style="top: 70%" />
  {:else if kind !== 'output'}
    <Handle type="source" position={Position.Right} />
  {/if}
</div>

<style>
  .agent-node {
    min-width: 180px;
    max-width: 240px;
    background: #151a26;
    border: 1px solid #2a3242;
    border-left: 4px solid var(--accent);
    border-radius: 10px;
    padding: 10px 12px;
    box-shadow: 0 4px 14px rgba(0, 0, 0, 0.4);
    font-size: 13px;
  }
  .agent-node.selected {
    border-color: var(--accent);
    box-shadow: 0 0 0 2px var(--accent);
  }
  .head {
    display: flex;
    align-items: center;
    gap: 8px;
    font-weight: 600;
  }
  .icon {
    font-size: 15px;
  }
  .label {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .sub {
    margin-top: 4px;
    font-size: 11px;
    color: #8a94ab;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .branch-labels {
    display: flex;
    justify-content: flex-end;
    gap: 6px;
    margin-top: 6px;
    font-size: 10px;
  }
  .branch-labels .true {
    color: #10b981;
  }
  .branch-labels .false {
    color: #ef4444;
  }
</style>
