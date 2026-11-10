import { nativelyApiBase } from '../services/nativelyApiBase';

export class CoreTavilySearchProvider {
  constructor(private readonly apiKey: string) {}

  async search(query: string, numResults = 5): Promise<unknown> {
    const response = await fetch('https://api.tavily.com/search', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ api_key: this.apiKey, query, max_results: numResults, search_depth: 'advanced' }),
    });
    if (!response.ok) throw new Error(`Tavily search failed (${response.status})`);
    const body: any = await response.json();
    return Array.isArray(body?.results) ? body.results : [];
  }
}

export class CoreNativelySearchProvider {
  public quotaExhausted = false;
  constructor(private readonly apiKey: string, private readonly trialToken?: string) {}

  async search(query: string, numResults = 5): Promise<unknown> {
    const headers: Record<string, string> = { 'content-type': 'application/json' };
    if (this.trialToken) headers['x-trial-token'] = this.trialToken;
    else headers['x-api-key'] = this.apiKey;
    const response = await fetch(`${nativelyApiBase()}/v1/search`, {
      method: 'POST', headers, body: JSON.stringify({ query, max_results: numResults }),
    });
    if (response.status === 402 || response.status === 429) this.quotaExhausted = true;
    if (!response.ok) throw new Error(`Natively search failed (${response.status})`);
    const body: any = await response.json();
    return Array.isArray(body) ? body : Array.isArray(body?.results) ? body.results : [];
  }
}
