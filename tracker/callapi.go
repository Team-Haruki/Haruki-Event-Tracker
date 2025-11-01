package tracker

import (
	"context"
	"fmt"
	"haruki-tracker/config"
	"time"

	"haruki-tracker/utils/model"

	"github.com/bytedance/sonic"
	"github.com/go-resty/resty/v2"
)

type HarukiSekaiAPIClient struct {
	apiEndpoint   string
	authorization string
	client        *resty.Client
}

func NewHarukiSekaiAPIClient(apiEndpoint, authorization string) *HarukiSekaiAPIClient {
	client := resty.New().
		SetTimeout(20*time.Second).
		SetHeader("User-Agent", fmt.Sprintf("Haruki-Event-Tracker/%s", config.Version))
	if authorization != "" {
		client.SetHeader("X-Haruki-Sekai-Token", authorization)
	}
	return &HarukiSekaiAPIClient{
		apiEndpoint:   apiEndpoint,
		authorization: authorization,
		client:        client,
	}
}

func (c *HarukiSekaiAPIClient) GetTop100(ctx context.Context, eventID int, server model.SekaiServerRegion) (*model.Top100RankingResponse, error) {
	url := fmt.Sprintf("%s/%s/event/%d/ranking-top100", c.apiEndpoint, server, eventID)
	resp, err := c.client.R().
		SetContext(ctx).
		Get(url)
	if err != nil {
		return nil, fmt.Errorf("failed to get top100: %w", err)
	}
	if resp.StatusCode() != 200 {
		return nil, fmt.Errorf("unexpected status code: %d", resp.StatusCode())
	}
	var response model.Top100RankingResponse
	if err := sonic.Unmarshal(resp.Body(), &response); err != nil {
		return nil, fmt.Errorf("failed to unmarshal response: %w", err)
	}
	return &response, nil
}

func (c *HarukiSekaiAPIClient) GetBorder(ctx context.Context, eventID int, server model.SekaiServerRegion) (*model.BorderRankingResponse, error) {
	url := fmt.Sprintf("%s/%s/event/%d/ranking-border", c.apiEndpoint, server, eventID)
	resp, err := c.client.R().
		SetContext(ctx).
		Get(url)
	if err != nil {
		return nil, fmt.Errorf("failed to get border: %w", err)
	}
	if resp.StatusCode() != 200 {
		return nil, fmt.Errorf("unexpected status code: %d", resp.StatusCode())
	}
	var response model.BorderRankingResponse
	if err := sonic.Unmarshal(resp.Body(), &response); err != nil {
		return nil, fmt.Errorf("failed to unmarshal response: %w", err)
	}
	return &response, nil
}
