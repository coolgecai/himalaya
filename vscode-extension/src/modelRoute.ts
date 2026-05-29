import * as vscode from 'vscode';

export interface ModelRouteState {
  model?: string;
  modelBackend?: string;
  modelSource?: 'cloud' | 'local';
  cloudBaseUrl?: string;
  cloudApiKey?: string;
  cloudModel?: string;
}

const modelRouteKey = 'himalayaCode.modelRoute.v1';
const cloudApiKeyKey = 'himalayaCode.modelRoute.cloudApiKey.v1';

export async function readModelRoute(context: vscode.ExtensionContext): Promise<ModelRouteState> {
  const route = context.globalState.get<ModelRouteState>(modelRouteKey, {});
  const cloudApiKey = await context.secrets.get(cloudApiKeyKey);

  return {
    ...route,
    ...(cloudApiKey ? { cloudApiKey } : {})
  };
}

export async function writeModelRoute(context: vscode.ExtensionContext, route: ModelRouteState): Promise<void> {
  const storedRoute: ModelRouteState = {
    model: route.model?.trim(),
    modelBackend: route.modelBackend?.trim(),
    modelSource: route.modelSource,
    cloudBaseUrl: route.cloudBaseUrl?.trim(),
    cloudModel: route.cloudModel?.trim()
  };

  await context.globalState.update(modelRouteKey, storedRoute);

  const apiKey = route.cloudApiKey?.trim() ?? '';
  if (apiKey) {
    await context.secrets.store(cloudApiKeyKey, apiKey);
  } else {
    await context.secrets.delete(cloudApiKeyKey);
  }
}