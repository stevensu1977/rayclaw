use std::sync::OnceLock;

use async_trait::async_trait;
use serde_json::json;

use super::{schema_object, Tool, ToolResult};
use crate::channels::feishu::{get_token, resolve_domain, FeishuChannelConfig};
use crate::config::Config;
use crate::llm_types::ToolDefinition;

fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to build HTTP client")
    })
}

/// Feishu document tool — read, create, and manipulate Feishu docs via the docx API.
pub struct FeishuDocTool {
    app_id: String,
    app_secret: String,
    base_url: String,
}

impl FeishuDocTool {
    pub fn new(config: &Config) -> Option<Self> {
        let cfg: FeishuChannelConfig = config.channel_config("feishu")?;
        Some(Self {
            app_id: cfg.app_id.clone(),
            app_secret: cfg.app_secret.clone(),
            base_url: resolve_domain(&cfg.domain),
        })
    }

    async fn get_token(&self) -> Result<String, String> {
        get_token(
            http_client(),
            &self.base_url,
            &self.app_id,
            &self.app_secret,
        )
        .await
    }

    /// GET helper with auth
    async fn api_get(&self, path: &str) -> Result<serde_json::Value, String> {
        let token = self.get_token().await?;
        let url = format!("{}{path}", self.base_url);
        let resp = http_client()
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?;
        let status = resp.status();
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("JSON parse error: {e}"))?;
        let code = body.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = body
                .get("msg")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            return Err(format!(
                "Feishu API error: code={code} msg={msg} status={status}"
            ));
        }
        Ok(body)
    }

    /// POST helper with auth + JSON body
    async fn api_post(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let token = self.get_token().await?;
        let url = format!("{}{path}", self.base_url);
        let resp = http_client()
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .json(body)
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?;
        let status = resp.status();
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("JSON parse error: {e}"))?;
        let code = body.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = body
                .get("msg")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            return Err(format!(
                "Feishu API error: code={code} msg={msg} status={status}"
            ));
        }
        Ok(body)
    }

    /// PATCH helper with auth + JSON body
    async fn api_patch(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let token = self.get_token().await?;
        let url = format!("{}{path}", self.base_url);
        let resp = http_client()
            .patch(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .json(body)
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?;
        let status = resp.status();
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("JSON parse error: {e}"))?;
        let code = body.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = body
                .get("msg")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            return Err(format!(
                "Feishu API error: code={code} msg={msg} status={status}"
            ));
        }
        Ok(body)
    }

    /// DELETE helper with auth
    async fn api_delete(&self, path: &str) -> Result<serde_json::Value, String> {
        let token = self.get_token().await?;
        let url = format!("{}{path}", self.base_url);
        let resp = http_client()
            .delete(&url)
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?;
        let status = resp.status();
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("JSON parse error: {e}"))?;
        let code = body.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = body
                .get("msg")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            return Err(format!(
                "Feishu API error: code={code} msg={msg} status={status}"
            ));
        }
        Ok(body)
    }

    // -----------------------------------------------------------------------
    // Action implementations
    // -----------------------------------------------------------------------

    /// Read a document's raw content (returns block tree as JSON).
    async fn read_doc(&self, document_id: &str) -> Result<String, String> {
        let resp = self
            .api_get(&format!(
                "/open-apis/docx/v1/documents/{document_id}/raw_content"
            ))
            .await?;
        let content = resp
            .pointer("/data/content")
            .cloned()
            .unwrap_or(json!(null));
        Ok(serde_json::to_string_pretty(&content).unwrap_or_default())
    }

    /// Get document metadata (title, owner, revision).
    async fn get_doc_meta(&self, document_id: &str) -> Result<String, String> {
        let resp = self
            .api_get(&format!("/open-apis/docx/v1/documents/{document_id}"))
            .await?;
        let doc = resp
            .pointer("/data/document")
            .cloned()
            .unwrap_or(json!(null));
        Ok(serde_json::to_string_pretty(&doc).unwrap_or_default())
    }

    /// List all blocks in a document (paginated).
    async fn list_blocks(
        &self,
        document_id: &str,
        page_token: Option<&str>,
        page_size: u32,
    ) -> Result<String, String> {
        let mut path =
            format!("/open-apis/docx/v1/documents/{document_id}/blocks?page_size={page_size}");
        if let Some(token) = page_token {
            path.push_str(&format!("&page_token={token}"));
        }
        let resp = self.api_get(&path).await?;
        let data = resp.get("data").cloned().unwrap_or(json!(null));
        Ok(serde_json::to_string_pretty(&data).unwrap_or_default())
    }

    /// Get a single block by ID.
    async fn get_block(&self, document_id: &str, block_id: &str) -> Result<String, String> {
        let resp = self
            .api_get(&format!(
                "/open-apis/docx/v1/documents/{document_id}/blocks/{block_id}"
            ))
            .await?;
        let block = resp.pointer("/data/block").cloned().unwrap_or(json!(null));
        Ok(serde_json::to_string_pretty(&block).unwrap_or_default())
    }

    /// Create a new document with a title, optionally in a specific folder.
    async fn create_doc(&self, title: &str, folder_token: Option<&str>) -> Result<String, String> {
        let mut body = json!({ "title": title });
        if let Some(folder) = folder_token {
            body["folder_token"] = json!(folder);
        }
        let resp = self.api_post("/open-apis/docx/v1/documents", &body).await?;
        let doc = resp
            .pointer("/data/document")
            .cloned()
            .unwrap_or(json!(null));
        Ok(serde_json::to_string_pretty(&doc).unwrap_or_default())
    }

    /// Create child blocks under a parent block.
    /// `children` is a JSON array of block definitions per Feishu docx API spec.
    async fn create_block(
        &self,
        document_id: &str,
        block_id: &str,
        index: Option<i64>,
        children: serde_json::Value,
    ) -> Result<String, String> {
        let mut body = json!({ "children": children });
        if let Some(idx) = index {
            body["index"] = json!(idx);
        }
        let resp = self
            .api_post(
                &format!("/open-apis/docx/v1/documents/{document_id}/blocks/{block_id}/children"),
                &body,
            )
            .await?;
        let data = resp.get("data").cloned().unwrap_or(json!(null));
        Ok(serde_json::to_string_pretty(&data).unwrap_or_default())
    }

    /// Update a block's content (e.g., replace text elements).
    async fn update_block(
        &self,
        document_id: &str,
        block_id: &str,
        update_body: serde_json::Value,
    ) -> Result<String, String> {
        let resp = self
            .api_patch(
                &format!("/open-apis/docx/v1/documents/{document_id}/blocks/{block_id}"),
                &update_body,
            )
            .await?;
        let data = resp.get("data").cloned().unwrap_or(json!(null));
        Ok(serde_json::to_string_pretty(&data).unwrap_or_default())
    }

    /// Delete a block from a document.
    async fn delete_block(&self, document_id: &str, block_id: &str) -> Result<String, String> {
        let resp = self
            .api_delete(&format!(
                "/open-apis/docx/v1/documents/{document_id}/blocks/{block_id}/children"
            ))
            .await?;
        let data = resp.get("data").cloned().unwrap_or(json!(null));
        Ok(serde_json::to_string_pretty(&data).unwrap_or_default())
    }
}

#[async_trait]
impl Tool for FeishuDocTool {
    fn name(&self) -> &str {
        "feishu_doc"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "feishu_doc".into(),
            description: "Feishu document operations: read, create, and edit documents via the Feishu/Lark docx API. \
                Supports reading document content, getting metadata, listing/creating/updating/deleting blocks, \
                and creating new documents."
                .into(),
            input_schema: schema_object(
                json!({
                    "action": {
                        "type": "string",
                        "enum": ["read_doc", "get_meta", "list_blocks", "get_block", "create_doc", "create_block", "update_block", "delete_block"],
                        "description": "The operation to perform:\n\
                            - read_doc: Read full document raw content\n\
                            - get_meta: Get document metadata (title, owner, revision)\n\
                            - list_blocks: List all blocks in a document (paginated)\n\
                            - get_block: Get a single block by ID\n\
                            - create_doc: Create a new empty document\n\
                            - create_block: Insert child blocks under a parent block\n\
                            - update_block: Update a block's content\n\
                            - delete_block: Delete a block's children"
                    },
                    "document_id": {
                        "type": "string",
                        "description": "The document ID (required for all actions except create_doc)"
                    },
                    "block_id": {
                        "type": "string",
                        "description": "The block ID (required for get_block, create_block, update_block, delete_block)"
                    },
                    "title": {
                        "type": "string",
                        "description": "Document title (for create_doc)"
                    },
                    "folder_token": {
                        "type": "string",
                        "description": "Optional folder token to create the document in (for create_doc)"
                    },
                    "children": {
                        "type": "array",
                        "description": "Array of block definitions to insert (for create_block). Each block follows Feishu docx block schema."
                    },
                    "index": {
                        "type": "integer",
                        "description": "Insert position index (for create_block, 0-based). Omit to append."
                    },
                    "update_body": {
                        "type": "object",
                        "description": "Block update payload (for update_block). Follow Feishu docx PATCH block schema."
                    },
                    "page_token": {
                        "type": "string",
                        "description": "Pagination token (for list_blocks)"
                    },
                    "page_size": {
                        "type": "integer",
                        "description": "Number of blocks per page (for list_blocks, default 50, max 500)"
                    }
                }),
                &["action"],
            ),
        }
    }

    async fn execute(&self, input: serde_json::Value) -> ToolResult {
        let action = match input.get("action").and_then(|v| v.as_str()) {
            Some(a) => a,
            None => return ToolResult::error("Missing required parameter: action".into()),
        };

        let document_id = input.get("document_id").and_then(|v| v.as_str());
        let block_id = input.get("block_id").and_then(|v| v.as_str());

        let result = match action {
            "read_doc" => {
                let doc_id = match document_id {
                    Some(id) => id,
                    None => {
                        return ToolResult::error(
                            "read_doc requires document_id".into(),
                        )
                    }
                };
                self.read_doc(doc_id).await
            }
            "get_meta" => {
                let doc_id = match document_id {
                    Some(id) => id,
                    None => {
                        return ToolResult::error(
                            "get_meta requires document_id".into(),
                        )
                    }
                };
                self.get_doc_meta(doc_id).await
            }
            "list_blocks" => {
                let doc_id = match document_id {
                    Some(id) => id,
                    None => {
                        return ToolResult::error(
                            "list_blocks requires document_id".into(),
                        )
                    }
                };
                let page_token = input.get("page_token").and_then(|v| v.as_str());
                let page_size = input
                    .get("page_size")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(50) as u32;
                self.list_blocks(doc_id, page_token, page_size).await
            }
            "get_block" => {
                let doc_id = match document_id {
                    Some(id) => id,
                    None => {
                        return ToolResult::error(
                            "get_block requires document_id".into(),
                        )
                    }
                };
                let blk_id = match block_id {
                    Some(id) => id,
                    None => {
                        return ToolResult::error(
                            "get_block requires block_id".into(),
                        )
                    }
                };
                self.get_block(doc_id, blk_id).await
            }
            "create_doc" => {
                let title = input
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Untitled");
                let folder_token = input.get("folder_token").and_then(|v| v.as_str());
                self.create_doc(title, folder_token).await
            }
            "create_block" => {
                let doc_id = match document_id {
                    Some(id) => id,
                    None => {
                        return ToolResult::error(
                            "create_block requires document_id".into(),
                        )
                    }
                };
                let blk_id = match block_id {
                    Some(id) => id,
                    None => {
                        return ToolResult::error(
                            "create_block requires block_id".into(),
                        )
                    }
                };
                let children = match input.get("children") {
                    Some(c) => c.clone(),
                    None => {
                        return ToolResult::error(
                            "create_block requires children array".into(),
                        )
                    }
                };
                let index = input.get("index").and_then(|v| v.as_i64());
                self.create_block(doc_id, blk_id, index, children).await
            }
            "update_block" => {
                let doc_id = match document_id {
                    Some(id) => id,
                    None => {
                        return ToolResult::error(
                            "update_block requires document_id".into(),
                        )
                    }
                };
                let blk_id = match block_id {
                    Some(id) => id,
                    None => {
                        return ToolResult::error(
                            "update_block requires block_id".into(),
                        )
                    }
                };
                let update_body = match input.get("update_body") {
                    Some(b) => b.clone(),
                    None => {
                        return ToolResult::error(
                            "update_block requires update_body object".into(),
                        )
                    }
                };
                self.update_block(doc_id, blk_id, update_body).await
            }
            "delete_block" => {
                let doc_id = match document_id {
                    Some(id) => id,
                    None => {
                        return ToolResult::error(
                            "delete_block requires document_id".into(),
                        )
                    }
                };
                let blk_id = match block_id {
                    Some(id) => id,
                    None => {
                        return ToolResult::error(
                            "delete_block requires block_id".into(),
                        )
                    }
                };
                self.delete_block(doc_id, blk_id).await
            }
            _ => {
                return ToolResult::error(format!(
                    "Unknown action: {action}. Valid: read_doc, get_meta, list_blocks, get_block, create_doc, create_block, update_block, delete_block"
                ))
            }
        };

        match result {
            Ok(content) => ToolResult::success(content),
            Err(e) => ToolResult::error(e),
        }
    }
}
