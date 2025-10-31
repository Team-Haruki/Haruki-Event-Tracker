package tracker

import (
	"context"
	"fmt"
	"time"

	"haruki-tracker/utils/model"

	"github.com/bytedance/sonic"
	"github.com/go-resty/resty/v2"
)

// HarukiSekaiAPIClient is a client for the Haruki Sekai API
type HarukiSekaiAPIClient struct {
	apiEndpoint   string
	authorization string
	client        *resty.Client
	userAgent     string
}

// NewHarukiSekaiAPIClient creates a new API client instance
func NewHarukiSekaiAPIClient(apiEndpoint, authorization string) *HarukiSekaiAPIClient {
	client := resty.New().
		SetTimeout(20*time.Second).
		SetHeader("User-Agent", "Haruki Event Tracker / v1.3.1")

	if authorization != "" {
		client.SetHeader("X-Haruki-Sekai-Token", authorization)
	}

	return &HarukiSekaiAPIClient{
		apiEndpoint:   apiEndpoint,
		authorization: authorization,
		client:        client,
		userAgent:     "Haruki Event Tracker / v1.3.1",
	}
}

// GetTop100 retrieves top 100 rankings for an event
func (c *HarukiSekaiAPIClient) GetTop100(ctx context.Context, eventID int, server model.SekaiServerRegion) (*model.Top100RankingResponse, error) {
	url := fmt.Sprintf("%s/%s/user/%%user_id/event/%d/ranking?rankingViewType=top100",
		c.apiEndpoint, server, eventID)

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

// GetBorder retrieves border rankings for an event
func (c *HarukiSekaiAPIClient) GetBorder(ctx context.Context, eventID int, server model.SekaiServerRegion) (*model.BorderRankingResponse, error) {
	url := fmt.Sprintf("%s/%s/event/%d/ranking-border",
		c.apiEndpoint, server, eventID)

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

// Close closes the HTTP client
func (c *HarukiSekaiAPIClient) Close() error {
	// resty client doesn't require explicit closing
	// Connection pooling is handled automatically
	return nil
}
