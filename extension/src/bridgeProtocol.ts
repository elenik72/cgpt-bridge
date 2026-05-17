export interface TestAskRequest {
  id: string;
  type: "test.ask";
  text: string;
  timeoutMs: number;
}

export interface DiagnoseRequest {
  id: string;
  type: "diagnose";
}

export type ExtensionRequest = TestAskRequest | DiagnoseRequest;

export interface TestAskOkResponse {
  id: string;
  ok: true;
  kind: "test.ask";
  text: string;
}

export interface DiagnoseOkResponse {
  id: string;
  ok: true;
  kind: "diagnose";
  diagnostics: unknown;
}

export type OkResponse = TestAskOkResponse | DiagnoseOkResponse;

export interface ErrorResponse {
  id: string;
  ok: false;
  errorCode: string;
  message: string;
}

export type ExtensionResponse = OkResponse | ErrorResponse;

export function newRequestId(): string {
  const rand = Math.random().toString(36).slice(2, 10);
  return `req_${Date.now().toString(36)}_${rand}`;
}
