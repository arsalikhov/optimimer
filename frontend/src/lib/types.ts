import type { Node, Edge } from '@xyflow/svelte';

export type NodeKind = 'trigger' | 'llm' | 'http' | 'condition' | 'datetime' | 'schedule' | 'output';

export interface Workflow {
  id: string;
  name: string;
  nodes: Node[];
  edges: Edge[];
  updated_at: string;
}

export interface NodeResult {
  node_id: string;
  node_type: string;
  status: 'ok' | 'error' | 'skipped';
  output: unknown;
  error?: string;
  ms: number;
}

export interface RunResponse {
  status: 'ok' | 'error';
  results: NodeResult[];
}

/** Catalog driving the palette, default data, and config-panel fields. */
export interface FieldDef {
  key: string;
  label: string;
  type: 'text' | 'textarea' | 'select';
  options?: string[];
  placeholder?: string;
}

export interface NodeDef {
  kind: NodeKind;
  label: string;
  icon: string;
  color: string;
  hint: string;
  defaults: Record<string, unknown>;
  fields: FieldDef[];
}

export const NODE_DEFS: Record<NodeKind, NodeDef> = {
  trigger: {
    kind: 'trigger',
    label: 'Trigger',
    icon: '⚡',
    color: '#f59e0b',
    hint: 'Starts the agent. Its payload is available as {{input}}.',
    defaults: { label: 'When triggered' },
    fields: [{ key: 'label', label: 'Name', type: 'text', placeholder: 'When triggered' }]
  },
  llm: {
    kind: 'llm',
    label: 'AI Step',
    icon: '🧠',
    color: '#8b5cf6',
    hint: 'Calls an LLM via OpenRouter. Nemotron is the cheap base; pick Claude Sonnet for strict/structured output.',
    defaults: {
      label: 'AI Step',
      model: 'nvidia/nemotron-3-super-120b-a12b',
      system: 'You are a helpful assistant.',
      prompt: 'Summarize this: {{input}}'
    },
    fields: [
      { key: 'label', label: 'Name', type: 'text' },
      {
        key: 'model',
        label: 'Model',
        type: 'select',
        options: [
          'nvidia/nemotron-3-super-120b-a12b',
          'nvidia/nemotron-3-super-120b-a12b:free',
          'anthropic/claude-sonnet-4.6',
          'openai/gpt-4o',
          'openai/gpt-4o-mini',
          'google/gemini-flash-1.5',
          'meta-llama/llama-3.1-70b-instruct'
        ]
      },
      { key: 'system', label: 'System prompt', type: 'textarea' },
      { key: 'prompt', label: 'Prompt', type: 'textarea', placeholder: 'Use {{input}} or {{nodeId.text}}' }
    ]
  },
  http: {
    kind: 'http',
    label: 'HTTP Action',
    icon: '🌐',
    color: '#06b6d4',
    hint: 'Calls an external API. Body/URL support {{templating}}.',
    defaults: { label: 'HTTP Action', method: 'GET', url: 'https://api.example.com', body: '', headers_json: '' },
    fields: [
      { key: 'label', label: 'Name', type: 'text' },
      { key: 'method', label: 'Method', type: 'select', options: ['GET', 'POST', 'PUT', 'DELETE'] },
      { key: 'url', label: 'URL', type: 'text', placeholder: 'https://...' },
      { key: 'body', label: 'Body (JSON)', type: 'textarea' },
      { key: 'headers_json', label: 'Headers — JSON object (optional)', type: 'textarea', placeholder: '{"Authorization": "Bearer {{env.MY_KEY}}"}' }
    ]
  },
  condition: {
    kind: 'condition',
    label: 'Condition',
    icon: '🔀',
    color: '#10b981',
    hint: 'Branches the flow. Routes to the true / false handle.',
    defaults: { label: 'If', left: '{{input}}', op: 'contains', right: '' },
    fields: [
      { key: 'label', label: 'Name', type: 'text' },
      { key: 'left', label: 'Left value', type: 'text' },
      {
        key: 'op',
        label: 'Operator',
        type: 'select',
        options: ['eq', 'ne', 'contains', 'gt', 'lt']
      },
      { key: 'right', label: 'Right value', type: 'text' }
    ]
  },
  datetime: {
    kind: 'datetime',
    label: 'Date/Time',
    icon: '🕒',
    color: '#0ea5e9',
    hint: 'Resolves an LLM-extracted when-token object into RFC3339 in code (no LLM date math). Outputs {{id.rfc3339}}, {{id.rfc3339_end}}, {{id.clear}}, {{id.far}}, {{id.human}}.',
    defaults: { label: 'Date/Time', spec: '{{parse.json.when}}', tz: '{{input.tz}}', default_hour: '9', duration_minutes: '0' },
    fields: [
      { key: 'label', label: 'Name', type: 'text' },
      { key: 'spec', label: 'When tokens — JSON {in_minutes,in_days,month,day,hour,...}', type: 'textarea' },
      { key: 'tz', label: 'Timezone (IANA)', type: 'text' },
      { key: 'default_hour', label: 'Default hour when no time given', type: 'text' },
      { key: 'duration_minutes', label: 'Duration for rfc3339_end', type: 'text' }
    ]
  },
  schedule: {
    kind: 'schedule',
    label: 'Schedule',
    icon: '⏰',
    color: '#f97316',
    hint: 'Sends a Telegram message at fire_at (RFC3339). Empty fire_at is a no-op.',
    defaults: {
      label: 'Schedule',
      fire_at: '',
      chat_id: '{{input.chat_id}}',
      message: ''
    },
    fields: [
      { key: 'label', label: 'Name', type: 'text' },
      { key: 'fire_at', label: 'Fire at (RFC3339)', type: 'text', placeholder: '{{parse.json.fire_at}}' },
      { key: 'chat_id', label: 'Telegram chat id', type: 'text' },
      { key: 'message', label: 'Telegram message', type: 'textarea' }
    ]
  },
  output: {
    kind: 'output',
    label: 'Output',
    icon: '📤',
    color: '#ef4444',
    hint: 'Final result of the run.',
    defaults: { label: 'Output', value: '{{input}}' },
    fields: [
      { key: 'label', label: 'Name', type: 'text' },
      { key: 'value', label: 'Value', type: 'textarea', placeholder: 'Use {{nodeId.text}}' }
    ]
  }
};
