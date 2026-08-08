// 由 cc-switch codexProviderPresets.ts 提取生成。
// 模板类预设 (baseUrl/model) 切换时程序化生成 config.toml;
// 手写类预设 (config) 整份作为 config.toml 底稿。

export type PresetCategory = "cn_official" | "aggregator" | "third_party";

export interface CodexffPreset {
  name: string;
  websiteUrl?: string;
  apiKeyUrl?: string;
  category: PresetCategory;
  icon?: string;
  iconColor?: string;
  /** 手写完整 TOML 底稿 (无此字段 = 程序化生成) */
  config?: string;
  baseUrl?: string;
  model?: string;
  reasoningEffort?: string;
  /** 供应商官方上下文窗口 (token); 无 = 用 400000 官方默认 */
  contextWindow?: number;
  /** 接口格式: "chat" = OpenAI Chat Completions (cc-switch apiFormat=openai_chat);
   *  "responses" = OpenAI Responses (cc-switch openai_responses / 默认)。
   *  我们没有 cc-switch 的 responses↔chat 本地路由, chat-only 上游必须写 chat,
   *  否则 Codex 用 responses 请求会失败 (智谱 Coding Plan 等)。 */
  wireApi?: "chat" | "responses";
}

export const codexffPresets: CodexffPreset[] = [
  {
    "name": "Kimi",
    "websiteUrl": "https://platform.kimi.com?aff=cc-switch",
    "apiKeyUrl": "https://platform.kimi.com/console/api-keys?aff=cc-switch",
    "category": "cn_official",
    "icon": "kimi",
    "iconColor": "#6366F1",
    "contextWindow": 262144,
    "baseUrl": "https://api.moonshot.cn/v1",
    "model": "kimi-k2.7-code",
    "reasoningEffort": "high",
    "wireApi": "chat"
  },
  {
    "name": "Kimi For Coding",
    "websiteUrl": "https://www.kimi.com/code/?aff=cc-switch",
    "apiKeyUrl": "https://www.kimi.com/code/?aff=cc-switch",
    "category": "cn_official",
    "icon": "kimi",
    "iconColor": "#6366F1",
    "contextWindow": 262144,
    "baseUrl": "https://api.kimi.com/coding/v1",
    "model": "kimi-for-coding",
    "reasoningEffort": "high",
    "wireApi": "chat"
  },
  {
    "name": "火山Agentplan",
    "websiteUrl": "https://www.volcengine.com/activity/codingplan?ac=MMAP8JTTCAQ2&rc=6J6FV5N2&utm_campaign=hw&utm_content=ccswitch&utm_medium=devrel_tool_web&utm_source=OWO&utm_term=ccswitch",
    "apiKeyUrl": "https://www.volcengine.com/activity/codingplan?ac=MMAP8JTTCAQ2&rc=6J6FV5N2&utm_campaign=hw&utm_content=ccswitch&utm_medium=devrel_tool_web&utm_source=OWO&utm_term=ccswitch",
    "category": "cn_official",
    "icon": "huoshan",
    "iconColor": "#3370FF",
    "contextWindow": 256000,
    "baseUrl": "https://ark.cn-beijing.volces.com/api/coding/v3",
    "model": "ark-code-latest",
    "reasoningEffort": "high"
  },
  {
    "name": "BytePlus",
    "websiteUrl": "https://www.byteplus.com/en/product/modelark?utm_campaign=hw&utm_content=ccswitch&utm_medium=devrel_tool_web&utm_source=OWO&utm_term=ccswitch",
    "apiKeyUrl": "https://www.byteplus.com/en/product/modelark?utm_campaign=hw&utm_content=ccswitch&utm_medium=devrel_tool_web&utm_source=OWO&utm_term=ccswitch",
    "category": "cn_official",
    "icon": "byteplus",
    "iconColor": "#3370FF",
    "contextWindow": 256000,
    "baseUrl": "https://ark.ap-southeast.bytepluses.com/api/coding/v3",
    "model": "ark-code-latest",
    "reasoningEffort": "high",
    "wireApi": "chat"
  },
  {
    "name": "DouBaoSeed",
    "websiteUrl": "https://console.volcengine.com/ark/region:ark+cn-beijing/apiKey?apikey=%7B%7D&utm_campaign=hw&utm_content=ccswitch&utm_medium=devrel_tool_web&utm_source=OWO&utm_term=ccswitch",
    "apiKeyUrl": "https://console.volcengine.com/ark/region:ark+cn-beijing/apiKey?apikey=%7B%7D&utm_campaign=hw&utm_content=ccswitch&utm_medium=devrel_tool_web&utm_source=OWO&utm_term=ccswitch",
    "category": "cn_official",
    "icon": "doubao",
    "iconColor": "#3370FF",
    "contextWindow": 262144,
    "baseUrl": "https://ark.cn-beijing.volces.com/api/v3",
    "model": "doubao-seed-2-1-pro-260628",
    "reasoningEffort": "high"
  },
  {
    "name": "DeepSeek",
    "websiteUrl": "https://platform.deepseek.com",
    "apiKeyUrl": "https://platform.deepseek.com/api_keys",
    "category": "cn_official",
    "icon": "deepseek",
    "iconColor": "#1E88E5",
    "contextWindow": 1048576,
    "baseUrl": "https://api.deepseek.com",
    "model": "deepseek-v4-flash",
    "reasoningEffort": "high"
  },
  {
    "name": "Zhipu GLM",
    "websiteUrl": "https://open.bigmodel.cn",
    "apiKeyUrl": "https://www.bigmodel.cn/claude-code?ic=RRVJPB5SII",
    "category": "cn_official",
    "icon": "zhipu",
    "iconColor": "#0F62FE",
    "contextWindow": 200000,
    "baseUrl": "https://open.bigmodel.cn/api/coding/paas/v4",
    "model": "glm-5.2",
    "reasoningEffort": "high",
    "wireApi": "chat"
  },
  {
    "name": "Zhipu GLM en",
    "websiteUrl": "https://z.ai",
    "apiKeyUrl": "https://z.ai/subscribe?ic=8JVLJQFSKB",
    "category": "cn_official",
    "icon": "zhipu",
    "iconColor": "#0F62FE",
    "contextWindow": 200000,
    "baseUrl": "https://api.z.ai/api/coding/paas/v4",
    "model": "glm-5.2",
    "reasoningEffort": "high",
    "wireApi": "chat"
  },
  {
    "name": "Baidu Qianfan Coding Plan",
    "websiteUrl": "https://cloud.baidu.com/product/qianfan_modelbuilder",
    "apiKeyUrl": "https://console.bce.baidu.com/qianfan/ais/console/applicationConsole/application",
    "category": "cn_official",
    "icon": "baidu",
    "iconColor": "#2932E1",
    "contextWindow": 131072,
    "baseUrl": "https://qianfan.baidubce.com/v2/coding",
    "model": "qianfan-code-latest",
    "reasoningEffort": "high",
    "wireApi": "chat"
  },
  {
    "name": "Bailian",
    "websiteUrl": "https://bailian.console.aliyun.com",
    "apiKeyUrl": "https://bailian.console.aliyun.com/#/api-key",
    "category": "cn_official",
    "icon": "bailian",
    "iconColor": "#624AFF",
    "contextWindow": 1048576,
    "baseUrl": "https://dashscope.aliyuncs.com/compatible-mode/v1",
    "model": "qwen3-coder-plus",
    "reasoningEffort": "high"
  },
  {
    "name": "Tencent Hunyuan",
    "websiteUrl": "https://cloud.tencent.com/product/tokenhub",
    "apiKeyUrl": "https://console.cloud.tencent.com/tokenhub/apikey",
    "category": "cn_official",
    "icon": "hunyuan",
    "iconColor": "#0055E9",
    "contextWindow": 256000,
    "baseUrl": "https://tokenhub.tencentmaas.com/v1",
    "model": "hy3",
    "reasoningEffort": "high"
  },
  {
    "name": "StepFun",
    "websiteUrl": "https://platform.stepfun.com/step-plan",
    "apiKeyUrl": "https://platform.stepfun.com/interface-key",
    "category": "cn_official",
    "icon": "stepfun",
    "iconColor": "#16D6D2",
    "contextWindow": 262144,
    "baseUrl": "https://api.stepfun.com/step_plan/v1",
    "model": "step-3.7-flash",
    "reasoningEffort": "high",
    "wireApi": "chat"
  },
  {
    "name": "StepFun en",
    "websiteUrl": "https://platform.stepfun.ai/step-plan",
    "apiKeyUrl": "https://platform.stepfun.ai/interface-key",
    "category": "cn_official",
    "icon": "stepfun",
    "iconColor": "#16D6D2",
    "contextWindow": 262144,
    "baseUrl": "https://api.stepfun.ai/step_plan/v1",
    "model": "step-3.7-flash",
    "reasoningEffort": "high",
    "wireApi": "chat"
  },
  {
    "name": "Longcat",
    "websiteUrl": "https://longcat.chat/platform",
    "apiKeyUrl": "https://longcat.chat/platform/api_keys",
    "category": "cn_official",
    "icon": "longcat",
    "iconColor": "#29E154",
    "contextWindow": 1048576,
    "baseUrl": "https://api.longcat.chat/openai/v1",
    "model": "LongCat-2.0",
    "reasoningEffort": "high"
  },
  {
    "name": "MiniMax",
    "websiteUrl": "https://platform.minimaxi.com",
    "apiKeyUrl": "https://platform.minimaxi.com/subscribe/coding-plan",
    "category": "cn_official",
    "icon": "minimax",
    "iconColor": "#FF6B6B",
    "contextWindow": 1000000,
    "baseUrl": "https://api.minimaxi.com/v1",
    "model": "MiniMax-M3",
    "reasoningEffort": "high"
  },
  {
    "name": "MiniMax en",
    "websiteUrl": "https://platform.minimax.io",
    "apiKeyUrl": "https://platform.minimax.io/subscribe/coding-plan",
    "category": "cn_official",
    "icon": "minimax",
    "iconColor": "#FF6B6B",
    "contextWindow": 1000000,
    "baseUrl": "https://api.minimax.io/v1",
    "model": "MiniMax-M3",
    "reasoningEffort": "high"
  },
  {
    "name": "BaiLing",
    "websiteUrl": "https://alipaytbox.yuque.com/sxs0ba/ling/get_started",
    "apiKeyUrl": "https://ling.tbox.cn/open",
    "category": "cn_official",
    "contextWindow": 262144,
    "baseUrl": "https://api.tbox.cn/api/llm/v1",
    "model": "Ling-2.6-1T",
    "reasoningEffort": "high",
    "wireApi": "chat"
  },
  {
    "name": "Xiaomi MiMo",
    "websiteUrl": "https://platform.xiaomimimo.com",
    "apiKeyUrl": "https://platform.xiaomimimo.com/#/console/api-keys",
    "category": "cn_official",
    "icon": "xiaomimimo",
    "iconColor": "#000000",
    "contextWindow": 1048576,
    "baseUrl": "https://api.xiaomimimo.com/v1",
    "model": "mimo-v2.5-pro",
    "reasoningEffort": "high"
  },
  {
    "name": "Xiaomi MiMo Token Plan (China)",
    "websiteUrl": "https://platform.xiaomimimo.com/#/token-plan",
    "apiKeyUrl": "https://platform.xiaomimimo.com/#/console/plan-manage",
    "category": "cn_official",
    "icon": "xiaomimimo",
    "iconColor": "#000000",
    "contextWindow": 1048576,
    "baseUrl": "https://token-plan-cn.xiaomimimo.com/v1",
    "model": "mimo-v2.5-pro",
    "reasoningEffort": "high"
  },
  {
    "name": "ZetaAPI",
    "websiteUrl": "https://zetaapi.ai",
    "apiKeyUrl": "https://zetaapi.ai/go/u117",
    "category": "aggregator",
    "icon": "zetaapi",
    "baseUrl": "https://api.zetaapi.ai/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "FennoAI",
    "websiteUrl": "https://api.fenno.ai",
    "apiKeyUrl": "https://api.fenno.ai/register?redirect=/purchase?tab=subscription%26group=16&aff=P9MR3D3PLCNL",
    "category": "aggregator",
    "icon": "fenno",
    "baseUrl": "https://api.fenno.ai",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "RunAPI",
    "websiteUrl": "https://runapi.co",
    "apiKeyUrl": "https://runapi.co/register?aff=iOKB",
    "category": "aggregator",
    "icon": "runapi",
    "baseUrl": "https://runapi.co/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "Shengsuanyun",
    "websiteUrl": "https://www.shengsuanyun.com/?from=CH_4HHXMRYF",
    "apiKeyUrl": "https://www.shengsuanyun.com/?from=CH_4HHXMRYF",
    "category": "aggregator",
    "icon": "shengsuanyun",
    "baseUrl": "https://router.shengsuanyun.com/api/v1",
    "model": "openai/gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "SubRouter",
    "websiteUrl": "https://subrouter.ai",
    "apiKeyUrl": "https://subrouter.ai/register?aff=l3ri",
    "category": "aggregator",
    "icon": "subrouter",
    "baseUrl": "https://subrouter.ai/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "Code0",
    "websiteUrl": "https://code0.ai",
    "apiKeyUrl": "https://code0.ai/agent/register/B2XHxGjGmRvqgznY",
    "category": "aggregator",
    "icon": "code0",
    "baseUrl": "https://code0.ai/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "TeamoRouter",
    "websiteUrl": "https://teamorouter.com",
    "apiKeyUrl": "https://teamorouter.com/?utm_source=cc_switch&utm_medium=referral&utm_campaign=ai_directory",
    "category": "aggregator",
    "icon": "teamorouter",
    "baseUrl": "https://api.teamorouter.com/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "SiliconFlow",
    "websiteUrl": "https://siliconflow.cn",
    "apiKeyUrl": "https://cloud.siliconflow.cn/i/YflgU2Ve",
    "category": "aggregator",
    "icon": "siliconflow",
    "iconColor": "#6E29F6",
    "contextWindow": 200000,
    "baseUrl": "https://api.siliconflow.cn/v1",
    "model": "Pro/MiniMaxAI/MiniMax-M2.7",
    "reasoningEffort": "high",
    "wireApi": "chat"
  },
  {
    "name": "SiliconFlow en",
    "websiteUrl": "https://siliconflow.com",
    "apiKeyUrl": "https://cloud.siliconflow.cn/i/YflgU2Ve",
    "category": "aggregator",
    "icon": "siliconflow",
    "iconColor": "#000000",
    "contextWindow": 200000,
    "baseUrl": "https://api.siliconflow.com/v1",
    "model": "MiniMaxAI/MiniMax-M2.7",
    "reasoningEffort": "high",
    "wireApi": "chat"
  },
  {
    "name": "NekoCode",
    "websiteUrl": "https://nekocode.ai",
    "apiKeyUrl": "https://nekocode.ai?aff=CCSWITCH",
    "category": "aggregator",
    "icon": "nekocode",
    "baseUrl": "https://nekocode.ai/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "A6API",
    "websiteUrl": "https://www.a6api.com",
    "apiKeyUrl": "https://a6api.com/register?aff=AqNr",
    "category": "aggregator",
    "icon": "a6api",
    "baseUrl": "https://api.a6api.com/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "AtlasCloud",
    "websiteUrl": "https://www.atlascloud.ai/console/coding-plan",
    "apiKeyUrl": "https://www.atlascloud.ai/console/coding-plan",
    "category": "aggregator",
    "icon": "atlascloud",
    "contextWindow": 200000,
    "config": "model_provider = \"custom\"\nmodel = \"zai-org/glm-5.1\"\ndisable_response_storage = true\n\n[model_providers.custom]\nname = \"AtlasCloud\"\nbase_url = \"https://api.atlascloud.ai/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = true",
    "baseUrl": "https://api.atlascloud.ai/v1",
    "model": "zai-org/glm-5.1",
    "wireApi": "chat"
  },
  {
    "name": "Compshare",
    "websiteUrl": "https://www.compshare.cn",
    "apiKeyUrl": "https://www.compshare.cn/coding-plan?ytag=GPU_YY_YX_git_cc-switch",
    "category": "aggregator",
    "icon": "ucloud",
    "iconColor": "#000000",
    "baseUrl": "https://api.modelverse.cn/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "Compshare Coding Plan",
    "websiteUrl": "https://www.compshare.cn",
    "apiKeyUrl": "https://www.compshare.cn/coding-plan?ytag=GPU_YY_YX_git_cc-switch",
    "category": "aggregator",
    "icon": "ucloud",
    "iconColor": "#000000",
    "baseUrl": "https://cp.compshare.cn/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "CCSub",
    "websiteUrl": "https://www.ccsub.net",
    "apiKeyUrl": "https://www.ccsub.net/register?ref=Y6Z8DXEA",
    "category": "aggregator",
    "icon": "ccsub",
    "baseUrl": "https://www.ccsub.net/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "DMXAPI",
    "websiteUrl": "https://www.dmxapi.cn",
    "category": "aggregator",
    "baseUrl": "https://www.dmxapi.cn/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "Qiniu",
    "websiteUrl": "https://s.qiniu.com/nMvAvy",
    "apiKeyUrl": "https://s.qiniu.com/nMvAvy",
    "category": "aggregator",
    "icon": "qiniu",
    "baseUrl": "https://api.qnaigc.com/bypass/openai/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "Amux",
    "websiteUrl": "https://amux.ai",
    "apiKeyUrl": "https://amux.ai",
    "category": "aggregator",
    "icon": "amux",
    "baseUrl": "https://api.amux.ai/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "ModelScope",
    "websiteUrl": "https://modelscope.cn",
    "apiKeyUrl": "https://modelscope.cn/my/myaccesstoken",
    "category": "aggregator",
    "icon": "modelscope",
    "iconColor": "#624AFF",
    "contextWindow": 200000,
    "baseUrl": "https://api-inference.modelscope.cn/v1",
    "model": "ZhipuAI/GLM-5.1",
    "reasoningEffort": "high",
    "wireApi": "chat"
  },
  {
    "name": "Novita AI",
    "websiteUrl": "https://novita.ai",
    "apiKeyUrl": "https://novita.ai",
    "category": "aggregator",
    "icon": "novita",
    "iconColor": "#000000",
    "contextWindow": 202800,
    "baseUrl": "https://api.novita.ai/openai/v1",
    "model": "zai-org/glm-5.1",
    "reasoningEffort": "high",
    "wireApi": "chat"
  },
  {
    "name": "Nvidia",
    "websiteUrl": "https://build.nvidia.com",
    "apiKeyUrl": "https://build.nvidia.com/settings/api-keys",
    "category": "aggregator",
    "icon": "nvidia",
    "iconColor": "#000000",
    "contextWindow": 262144,
    "baseUrl": "https://integrate.api.nvidia.com/v1",
    "model": "moonshotai/kimi-k2.5",
    "reasoningEffort": "high",
    "wireApi": "chat"
  },
  {
    "name": "AiHubMix",
    "websiteUrl": "https://aihubmix.com",
    "category": "aggregator",
    "icon": "aihubmix",
    "iconColor": "#006FFB",
    "baseUrl": "https://aihubmix.com/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "CherryIN",
    "websiteUrl": "https://open.cherryin.ai",
    "apiKeyUrl": "https://open.cherryin.ai/console/token",
    "category": "aggregator",
    "icon": "cherryin",
    "baseUrl": "https://open.cherryin.net/v1",
    "model": "openai/gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "PIPELLM",
    "websiteUrl": "https://code.pipellm.ai",
    "apiKeyUrl": "https://code.pipellm.ai/login?ref=uvw650za",
    "category": "aggregator",
    "icon": "pipellm",
    "config": "model_provider = \"custom\"\nmodel = \"gpt-5.6-sol\"\nmodel_reasoning_effort = \"medium\"\ndisable_response_storage = true\n\n[model_providers.custom]\nname = \"PIPELLM\"\nwire_api = \"responses\"\nrequires_openai_auth = true\nbase_url = \"https://cc-api.pipellm.ai/v1\"",
    "baseUrl": "https://cc-api.pipellm.ai/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "medium"
  },
  {
    "name": "OpenRouter",
    "websiteUrl": "https://openrouter.ai",
    "apiKeyUrl": "https://openrouter.ai/keys",
    "category": "aggregator",
    "icon": "openrouter",
    "iconColor": "#6566F1",
    "baseUrl": "https://openrouter.ai/api/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "TheRouter",
    "websiteUrl": "https://therouter.ai",
    "apiKeyUrl": "https://dashboard.therouter.ai",
    "category": "aggregator",
    "baseUrl": "https://api.therouter.ai/v1",
    "model": "openai/gpt-5.3-codex",
    "reasoningEffort": "high"
  },
  {
    "name": "PackyCode",
    "websiteUrl": "https://www.packyapi.ai",
    "apiKeyUrl": "https://www.packyapi.ai/register?aff=cc-switch",
    "category": "third_party",
    "icon": "packycode",
    "baseUrl": "https://www.packyapi.ai/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "APINebula",
    "websiteUrl": "https://apinebula.ai",
    "apiKeyUrl": "https://apinebula.ai/VjM74M",
    "category": "third_party",
    "icon": "apinebula",
    "config": "model_provider = \"custom\"\nmodel = \"gpt-5.6-sol\"\nreview_model = \"gpt-5.6-sol\"\nmodel_reasoning_effort = \"high\"\ndisable_response_storage = true\n\n[model_providers.custom]\nname = \"APINebula\"\nbase_url = \"https://apinebula.ai/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = true",
    "baseUrl": "https://apinebula.ai/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "AICodeMirror",
    "websiteUrl": "https://www.aicodemirror.ai",
    "apiKeyUrl": "https://www.aicodemirror.ai/register?invitecode=9915W3",
    "icon": "aicodemirror",
    "iconColor": "#000000",
    "baseUrl": "https://api.aicodemirror.ai/api/codex/backend-api/codex",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high",
    "category": "third_party"
  },
  {
    "name": "PatewayAI",
    "websiteUrl": "https://pateway.ai",
    "apiKeyUrl": "https://pateway.ai/?ch=etzpm8&aff=WB6M6F67#/",
    "category": "third_party",
    "icon": "pateway",
    "baseUrl": "https://api.pateway.ai/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "AIGoCode",
    "websiteUrl": "https://aigocode.app",
    "apiKeyUrl": "https://aigocode.app/invite/CC-SWITCH",
    "category": "third_party",
    "icon": "aigocode",
    "iconColor": "#5B7FFF",
    "baseUrl": "https://api.aigocode.app",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "AICoding",
    "websiteUrl": "https://aicoding.inc",
    "apiKeyUrl": "https://aicoding.inc/i/CCSWITCH",
    "icon": "aicoding",
    "iconColor": "#000000",
    "baseUrl": "https://api.aicoding.inc",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high",
    "category": "third_party"
  },
  {
    "name": "APIKEY.FUN",
    "websiteUrl": "https://apikey.fun",
    "apiKeyUrl": "https://apikey.fun/register?aff=CCSwitch",
    "category": "third_party",
    "icon": "apikeyfun",
    "config": "model_provider = \"custom\"\nmodel = \"gpt-5.6-sol\"\nreview_model = \"gpt-5.6-sol\"\nmodel_reasoning_effort = \"high\"\ndisable_response_storage = true\n\n[model_providers.custom]\nname = \"APIKEY.FUN\"\nbase_url = \"https://api.apikey.fun/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = true",
    "baseUrl": "https://api.apikey.fun/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "ClaudeCN",
    "websiteUrl": "https://claudecn.top",
    "apiKeyUrl": "https://claudecn.ai/register?aff=HEL9",
    "category": "third_party",
    "icon": "claudecn",
    "baseUrl": "https://claudecn.top/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "SSSAiCode",
    "websiteUrl": "https://sssaicodeapi.com",
    "apiKeyUrl": "https://sssaicodeapi.com/register?ref=DCP0SM",
    "category": "third_party",
    "icon": "sssaicode",
    "iconColor": "#000000",
    "baseUrl": "https://node-hk.sssaicodeapi.com/api/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "Micu",
    "websiteUrl": "https://www.micuapi.ai",
    "apiKeyUrl": "https://www.micuapi.ai/register?aff=aOYQ",
    "category": "third_party",
    "icon": "micu",
    "iconColor": "#000000",
    "baseUrl": "https://www.micuapi.ai/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "RightCode",
    "websiteUrl": "https://www.rightapi.ai",
    "apiKeyUrl": "https://www.rightapi.ai/register?aff=CCSWITCH",
    "category": "third_party",
    "icon": "rc",
    "iconColor": "#E96B2C",
    "baseUrl": "https://www.rightapi.ai/codex/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "ETok.ai",
    "websiteUrl": "https://etok.ai",
    "apiKeyUrl": "https://etok.ai",
    "category": "third_party",
    "icon": "etok",
    "iconColor": "#000000",
    "baseUrl": "https://api.etok.ai/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "Cubence",
    "websiteUrl": "https://cubence.com",
    "apiKeyUrl": "https://cubence.com/signup?code=CCSWITCH&source=ccs",
    "category": "third_party",
    "icon": "cubence",
    "iconColor": "#000000",
    "baseUrl": "https://api.cubence.com/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "CrazyRouter",
    "websiteUrl": "https://www.crazyrouter.com",
    "apiKeyUrl": "https://www.crazyrouter.com/register?aff=OZcm&ref=cc-switch",
    "icon": "crazyrouter",
    "iconColor": "#000000",
    "baseUrl": "https://cn.crazyrouter.com/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high",
    "category": "third_party"
  },
  {
    "name": "SudoCode.chat",
    "websiteUrl": "https://sudocode.chat",
    "apiKeyUrl": "https://sudocode.chat/sign-up?aff=CC-SWITCH&utm_source=cc-switch&utm_medium=sponsor&utm_campaign=ccswitch",
    "category": "third_party",
    "icon": "sudocode",
    "config": "model_provider = \"custom\"\nmodel = \"gpt-5.6-sol\"\nreview_model = \"gpt-5.6-sol\"\nmodel_reasoning_effort = \"high\"\ndisable_response_storage = true\n\n[model_providers.custom]\nname = \"SudoCode\"\nbase_url = \"https://api.sudocode.chat/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = true",
    "baseUrl": "https://api.sudocode.chat/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "SudoCode.us",
    "websiteUrl": "https://sudocode.us",
    "apiKeyUrl": "https://sudocode.us",
    "category": "third_party",
    "icon": "sudocode-us",
    "config": "model_provider = \"custom\"\nmodel = \"gpt-5.6-sol\"\nreview_model = \"gpt-5.6-sol\"\nmodel_reasoning_effort = \"high\"\ndisable_response_storage = true\nmodel_verbosity = \"high\"\n\n[model_providers.custom]\nname = \"sudocode\"\nbase_url = \"https://sudocode.us/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = true",
    "baseUrl": "https://sudocode.us/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "Azure OpenAI",
    "websiteUrl": "https://learn.microsoft.com/en-us/azure/ai-foundry/openai/how-to/codex",
    "category": "third_party",
    "icon": "codex",
    "iconColor": "#0078D4",
    "config": "model_provider = \"custom\"\nmodel = \"gpt-5.6-sol\"\nmodel_reasoning_effort = \"high\"\ndisable_response_storage = true\n\n[model_providers.custom]\nname = \"Azure OpenAI\"\nbase_url = \"https://YOUR_RESOURCE_NAME.openai.azure.com/openai\"\nenv_key = \"OPENAI_API_KEY\"\nquery_params = { \"api-version\" = \"2025-04-01-preview\" }\nwire_api = \"responses\"\nrequires_openai_auth = true",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "xAI (Grok)",
    "websiteUrl": "https://x.ai/api",
    "apiKeyUrl": "https://console.x.ai",
    "category": "third_party",
    "icon": "xai",
    "iconColor": "#000000",
    "contextWindow": 500000,
    "baseUrl": "https://api.x.ai/v1",
    "model": "grok-4.5",
    "reasoningEffort": "high"
  },
  {
    "name": "OpenCode Go",
    "websiteUrl": "https://opencode.ai/go",
    "apiKeyUrl": "https://opencode.ai/go?ref=2YTRG2NGTX",
    "category": "third_party",
    "icon": "opencode",
    "iconColor": "#211E1E",
    "contextWindow": 204800,
    "baseUrl": "https://opencode.ai/zen/go/v1",
    "model": "glm-5.2",
    "reasoningEffort": "high",
    "wireApi": "chat"
  },
  {
    "name": "RelaxyCode",
    "websiteUrl": "https://www.relaxycode.com",
    "apiKeyUrl": "https://www.relaxycode.com/register",
    "category": "third_party",
    "icon": "relaxcode",
    "baseUrl": "https://www.relaxycode.com/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  },
  {
    "name": "E-FlowCode",
    "websiteUrl": "https://e-flowcode.cc",
    "apiKeyUrl": "https://e-flowcode.cc",
    "category": "third_party",
    "icon": "eflowcode",
    "iconColor": "#000000",
    "config": "model_provider = \"custom\"\nmodel = \"gpt-5.6-sol\"\nmodel_reasoning_effort = \"high\"\ndisable_response_storage = true\npersonality = \"pragmatic\"\n\n[model_providers.custom]\nname = \"E-FlowCode\"\nbase_url = \"https://e-flowcode.cc/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = true\nmodel_context_window = 1000000\nmodel_auto_compact_token_limit = 9000000",
    "baseUrl": "https://e-flowcode.cc/v1",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "high"
  }
];

export const CATEGORY_LABELS: Record<PresetCategory, string> = {
  cn_official: "国产官方",
  aggregator: "聚合服务",
  third_party: "第三方中转",
};
