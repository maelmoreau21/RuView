// Backend Detection Utility

import { API_CONFIG } from '../config/api.config.js';

export class BackendDetector {
  constructor() {
    this.isBackendAvailable = null;
    this.lastCheck = 0;
    this.checkInterval = 30000;
    this.sensingOnlyMode = false;
  }

  async checkBackendAvailability() {
    const now = Date.now();
    if (this.isBackendAvailable !== null && (now - this.lastCheck) < this.checkInterval) {
      return this.isBackendAvailable;
    }

    try {
      const controller = new AbortController();
      const timeoutId = setTimeout(() => controller.abort(), 3000);
      const response = await fetch(`${API_CONFIG.BASE_URL}/health/live`, {
        method: 'GET',
        signal: controller.signal,
        headers: { Accept: 'application/json' },
      });

      clearTimeout(timeoutId);
      this.isBackendAvailable = response.ok;
      this.lastCheck = now;
      return this.isBackendAvailable;
    } catch {
      this.isBackendAvailable = false;
      this.lastCheck = now;
      return false;
    }
  }

  async shouldUseMockServer() {
    if (API_CONFIG.MOCK_SERVER.ENABLED) {
      console.warn('Mock backends are disabled in the RuvSense production UI');
    }
    return false;
  }

  async getBaseUrl() {
    await this.shouldUseMockServer();
    return API_CONFIG.BASE_URL;
  }

  forceCheck() {
    this.isBackendAvailable = null;
    this.lastCheck = 0;
  }
}

export const backendDetector = new BackendDetector();
