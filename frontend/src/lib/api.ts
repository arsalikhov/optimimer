import type { Node, Edge } from '@xyflow/svelte';
import type { RunResponse, Workflow } from './types';

const BASE = '/api';

async function j<T>(res: Response): Promise<T> {
  if (!res.ok) throw new Error(`${res.status} ${res.statusText}: ${await res.text()}`);
  return res.json() as Promise<T>;
}

export const api = {
  list: () => fetch(`${BASE}/workflows`).then(j<Workflow[]>),

  get: (id: string) => fetch(`${BASE}/workflows/${id}`).then(j<Workflow>),

  create: (name: string, nodes: Node[], edges: Edge[]) =>
    fetch(`${BASE}/workflows`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ name, nodes, edges })
    }).then(j<Workflow>),

  update: (id: string, name: string, nodes: Node[], edges: Edge[]) =>
    fetch(`${BASE}/workflows/${id}`, {
      method: 'PUT',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ name, nodes, edges })
    }).then(j<Workflow>),

  remove: (id: string) => fetch(`${BASE}/workflows/${id}`, { method: 'DELETE' }),

  run: (id: string, workflow: { name: string; nodes: Node[]; edges: Edge[] }, input: unknown) =>
    fetch(`${BASE}/workflows/${id || 'inline'}/run`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        workflow: { id: id || 'inline', updated_at: '', ...workflow },
        input
      })
    }).then(j<RunResponse>)
};
