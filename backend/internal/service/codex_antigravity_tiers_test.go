package service

import (
	"context"
	"net/http"
	"testing"

	"github.com/Wei-Shaw/sub2api/internal/config"
	"github.com/stretchr/testify/require"
	"github.com/tidwall/gjson"
)

func antigravityTierTestMapping() map[string]any {
	return map[string]any{
		"gemini-4-hydrogen":        "gemini-4-hydrogen",
		"gemini-4-hydrogen-low":    "gemini-4-hydrogen-low",
		"gemini-4-hydrogen-medium": "gemini-4-hydrogen-medium",
		"gemini-4-hydrogen-high":   "gemini-4-hydrogen-high",
		"gemini-4-hydrogen-xhigh":  "gemini-4-hydrogen-xhigh",
	}
}

func TestCodexAntigravityTierCatalog(t *testing.T) {
	const groupID = 793
	account := newAntigravityCompatAccount(AccountTypeOAuth)
	account.Schedulable = true
	account.Credentials["model_mapping"] = antigravityTierTestMapping()
	account.SetUpstreamModelMetadataSnapshot(UpstreamModelMetadataSnapshot{
		Models: map[string]UpstreamModelMetadata{
			"gemini-4-hydrogen-high": {ID: "gemini-4-hydrogen-high", InputModalities: []string{"text", "image"}},
		},
	})
	svc := &GatewayService{accountRepo: codexModelsVisibilityAccountRepo{
		byGroup: map[int64][]Account{groupID: {*account}},
	}}
	group := &Group{ID: groupID, Platform: PlatformAntigravity}
	modelIDs := []string{"gemini-4-hydrogen", "gemini-4-hydrogen-low", "gemini-4-hydrogen-medium", "gemini-4-hydrogen-high", "gemini-4-hydrogen-xhigh"}
	body, err := svc.BuildCodexModelsManifestForGroup(context.Background(), group, "", modelIDs)
	require.NoError(t, err)
	require.Equal(t, []string{"gemini-4-hydrogen"}, codexManifestModelSlugs(t, body))
	model := decodeCodexManifestModels(t, body)[0]
	require.Equal(t, []string{"low", "medium", "high", "xhigh"}, effortsFromManifestModel(t, model))
	require.Equal(t, "high", model["default_reasoning_level"])

	body, err = svc.BuildCodexModelsManifestForGroup(context.Background(), group, PlatformAntigravity, modelIDs)
	require.NoError(t, err)
	require.ElementsMatch(t, modelIDs, codexManifestModelSlugs(t, body))

	body, err = svc.BuildCodexModelsManifestForGroup(context.Background(), group, "", modelIDs[4:])
	require.NoError(t, err)
	require.Equal(t, modelIDs[4:], codexManifestModelSlugs(t, body))

	other := Account{ID: 794, Platform: PlatformOpenAI, Type: AccountTypeAPIKey, Status: StatusActive, Schedulable: true}
	svc.accountRepo = codexModelsVisibilityAccountRepo{byGroup: map[int64][]Account{groupID: {*account, other}}}
	group.Platform = PlatformComposite
	body, err = svc.BuildCodexModelsManifestForGroup(context.Background(), group, "", modelIDs)
	require.NoError(t, err)
	require.Equal(t, modelIDs[:1], codexManifestModelSlugs(t, body))

	other.Credentials = map[string]any{"model_mapping": map[string]any{modelIDs[4]: "gpt-5"}}
	svc.accountRepo = codexModelsVisibilityAccountRepo{byGroup: map[int64][]Account{groupID: {*account, other}}}
	body, err = svc.BuildCodexModelsManifestForGroup(context.Background(), group, "", modelIDs)
	require.NoError(t, err)
	require.ElementsMatch(t, []string{modelIDs[0], modelIDs[4]}, codexManifestModelSlugs(t, body))
}

func TestCodexAntigravityTierResponses(t *testing.T) {
	for _, tc := range []struct {
		name, request, upstreamModel string
	}{
		{"extra high", `{"model":"gemini-4-hydrogen","reasoning":{"effort":"xhigh"},"stream":true,"input":"hello"}`, "gemini-4-hydrogen-xhigh"},
		{"explicit tier", `{"model":"gemini-4-hydrogen-medium","stream":true,"input":"hello"}`, "gemini-4-hydrogen-medium"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			upstream := &queuedHTTPUpstreamStub{responses: []*http.Response{antigravityCompatSuccessResponse()}}
			svc := newAntigravityCompatService(config.GatewayConfig{MaxLineSize: defaultMaxLineSize}, upstream)
			account := newAntigravityCompatAccount(AccountTypeOAuth)
			account.Credentials["model_mapping"] = antigravityTierTestMapping()
			body := []byte(tc.request)
			c, rec := newAntigravityCompatContext(http.MethodPost, "/v1/responses", body)
			_, err := svc.ForwardAsResponses(context.Background(), c, account, body, nil)
			require.NoError(t, err)
			require.Equal(t, http.StatusOK, rec.Code)
			require.Len(t, upstream.requestBodies, 1)
			wire := gjson.ParseBytes(upstream.requestBodies[0])
			require.Equal(t, tc.upstreamModel, wire.Get("model").String())
			require.False(t, wire.Get("request.generationConfig.thinkingConfig.thinkingLevel").Exists())
			require.False(t, wire.Get("request.generationConfig.thinkingConfig.thinkingBudget").Exists())
		})
	}
}

// 2026-11-21：验证新增模型目录只公布账号确实支持的档位，并保留管理员固定别名和路由。
func TestCodexAntigravityTierCatalogPreservesConfiguredBoundaries(t *testing.T) {
	for _, base := range []string{"gemini-4-neon", "gemini-4-hydrogen"} {
		t.Run(base, func(t *testing.T) {
			for _, mode := range []string{"self_mapping", "fixed_alias", "account_routing", "partial_tiers"} {
				t.Run(mode, func(t *testing.T) {
					const groupID = 795
					account := newAntigravityCompatAccount(AccountTypeOAuth)
					account.Schedulable = true
					mapping := map[string]any{base: base}
					models := []string{base}
					levels := []string{"low", "medium", "high", "xhigh"}
					if mode == "partial_tiers" {
						levels = []string{"low", "high"}
					}
					for _, tier := range levels {
						model := base + "-" + tier
						mapping[model] = model
						models = append(models, model)
					}
					if mode == "fixed_alias" {
						mapping[base] = base + "-high"
					}
					account.Credentials["model_mapping"] = mapping
					group := &Group{ID: groupID, Platform: PlatformAntigravity}
					if mode == "account_routing" {
						group.ModelRoutingEnabled = true
						group.ModelRouting = map[string][]int64{base: {account.ID}}
					}
					svc := &GatewayService{accountRepo: codexModelsVisibilityAccountRepo{
						byGroup: map[int64][]Account{groupID: {*account}},
					}}
					body, err := svc.BuildCodexModelsManifestForGroup(context.Background(), group, "", models)
					require.NoError(t, err)
					if mode == "fixed_alias" || mode == "account_routing" {
						require.ElementsMatch(t, models, codexManifestModelSlugs(t, body))
					} else {
						require.Equal(t, []string{base}, codexManifestModelSlugs(t, body))
						model := decodeCodexManifestModels(t, body)[0]
						require.Equal(t, levels, effortsFromManifestModel(t, model))
						require.Equal(t, "high", model["default_reasoning_level"])
					}
				})
			}
		})
	}
}

func TestCodexAntigravityTierCatalogPreservesKnownMetadataLimits(t *testing.T) {
	for _, base := range []string{"gemini-3.8-flash", "gemini-4-neon", "gemini-4-hydrogen"} {
		for _, tc := range []struct {
			name                        string
			tier                        *UpstreamModelMetadata
			baseModalities              []string
			wantContext, wantMaxContext int64
			wantModalities              []any
		}{
			{
				name:        "missing tier snapshots retain base capabilities",
				wantContext: 32768, wantMaxContext: 65536, wantModalities: []any{"text", "image"},
			},
			{
				name:        "smaller tier constrains base capabilities",
				tier:        &UpstreamModelMetadata{ContextWindow: 16384, MaxContextWindow: 24576, InputModalities: []string{"text"}},
				wantContext: 16384, wantMaxContext: 24576, wantModalities: []any{"text"},
			},
			{
				name:           "larger tier cannot expand base limits",
				tier:           &UpstreamModelMetadata{ContextWindow: 131072, MaxContextWindow: 262144, InputModalities: []string{"text", "image", "audio"}},
				baseModalities: []string{"text"},
				wantContext:    32768, wantMaxContext: 65536, wantModalities: []any{"text"},
			},
			{
				name:        "partial tier narrows only known fields",
				tier:        &UpstreamModelMetadata{InputModalities: []string{"text"}},
				wantContext: 32768, wantMaxContext: 65536, wantModalities: []any{"text"},
			},
			{
				name:        "tier maximum also caps default context",
				tier:        &UpstreamModelMetadata{MaxContextWindow: 8192},
				wantContext: 8192, wantMaxContext: 8192, wantModalities: []any{"text", "image"},
			},
		} {
			t.Run(base+"/"+tc.name, func(t *testing.T) {
				const groupID = 796
				account := newAntigravityCompatAccount(AccountTypeOAuth)
				account.Schedulable = true
				models := []string{base, base + "-low", base + "-high"}
				mapping := map[string]any{}
				for _, model := range models {
					mapping[model] = model
				}
				account.Credentials["model_mapping"] = mapping
				modalities := tc.baseModalities
				if len(modalities) == 0 {
					modalities = []string{"text", "image"}
				}
				snapshot := UpstreamModelMetadataSnapshot{Models: map[string]UpstreamModelMetadata{
					base: {ID: base, ContextWindow: 32768, MaxContextWindow: 65536, InputModalities: modalities},
				}}
				if tc.tier != nil {
					snapshot.Models[base+"-high"] = *tc.tier
				}
				account.SetUpstreamModelMetadataSnapshot(snapshot)
				svc := &GatewayService{accountRepo: codexModelsVisibilityAccountRepo{
					byGroup: map[int64][]Account{groupID: {*account}},
				}}
				body, err := svc.BuildCodexModelsManifestForGroup(context.Background(), &Group{ID: groupID, Platform: PlatformAntigravity}, "", models)
				require.NoError(t, err)
				require.Equal(t, []string{base}, codexManifestModelSlugs(t, body))
				model := decodeCodexManifestModels(t, body)[0]
				require.EqualValues(t, tc.wantContext, model["context_window"])
				require.EqualValues(t, tc.wantMaxContext, model["max_context_window"])
				require.Equal(t, tc.wantModalities, model["input_modalities"])
			})
		}
	}
}
